//! xdg-desktop-portal ScreenCast-Verhandlung (Wayland).
//!
//! Flow (wie GSR `capture/portal.c`): `CreateSession` → `SelectSources`
//! (Monitor/Window, Cursor-Mode) → `Start` (liefert PipeWire-`node_id`) →
//! `OpenPipeWireRemote` (liefert den fd für `pw_context_connect_fd`).
//!
//! ashpd ist async (zbus/tokio) — wir scopen einen tokio-Runtime NUR für diese
//! Verhandlung; danach läuft PipeWire synchron (MainLoop). User-Abbruch des
//! Portal-Dialogs → Exit-Code 60 (GSR `PORTAL_CAPTURE_CANCELED_BY_USER_EXIT_CODE`).
//!
//! niri nutzt hier den GNOME-Backend (niri implementiert
//! `org.gnome.Mutter.ScreenCast`), konfiguriert via
//! `~/.config/xdg-desktop-portal/portals.conf` (ScreenCast=gnome).

use std::os::fd::OwnedFd;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

/// Exit-Code bei User-Abbruch des Portal-Dialogs (GSR-Konvention).
pub const EXIT_PORTAL_CANCELED: i32 = 60;

/// Prozess-globale Tokio-Runtime für ALLE Portal-Verhandlungen.
///
/// Früher baute `open()` pro Aufruf eine eigene `current_thread`-Runtime und ließ
/// sie am Ende fallen. ashpd spricht aber über `zbus`, dessen Session-Bus-
/// Verbindung PROZESSWEIT gecacht ist (`zbus::Connection::session()` in einer
/// statischen `OnceCell`) — deren I/O-Treiber-Task lebt auf der Runtime, die beim
/// ERSTEN Aufruf aktiv war. Wird diese Runtime gedroppt, stirbt der Treiber,
/// während die Verbindung im Cache liegen bleibt. Der zweite Stream bekam so eine
/// tote Verbindung: `Start` (Portal-Dialog) sendete zwar, aber niemand las die
/// Antwort → Hänger direkt nach „öffne Portal-Dialog". Eine einzige dauerhafte
/// Multi-Thread-Runtime hält den Treiber über alle Streams am Leben.
fn portal_runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("portal tokio runtime bauen")
    })
}

/// Ergebnis der Portal-Verhandlung: PipeWire-fd + node_id + Quell-Größe.
pub struct PortalSession {
    /// fd für `pw_context_connect_fd`. Muss offen bleiben, solange der
    /// PipeWire-Stream läuft.
    pub pw_fd: OwnedFd,
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    /// Restore-Token für PersistMode (Wiederverwendung ohne Dialog beim
    /// nächsten Start). Noch nicht persistiert — kommt mit dem Settings-Store.
    pub restore_token: Option<String>,
    /// Schließt die Portal-Session beim Drop (Stream-Ende ODER Fehlerpfad).
    /// Ohne explizites `Close` bleibt die Session im Compositor registriert,
    /// solange der Sidecar-Prozess lebt (die zbus-Verbindung ist prozessweit
    /// gecacht) — KDE zeigt dann dauerhaft das rote „Bildschirm wird
    /// aufgenommen"-Tray-Symbol, eines pro geleakter Session. GSR hat das
    /// Problem nicht, weil dort der Prozess pro Aufnahme endet.
    _close_guard: SessionCloseGuard,
}

/// Hält den oneshot-Sender; sein Drop weckt den Close-Task im portal_runtime.
struct SessionCloseGuard(#[allow(dead_code)] oneshot::Sender<()>);

/// Verhandle eine ScreenCast-Session. Öffnet den Portal-Dialog (User wählt
/// Quelle). `show_cursor=true` → Cursor eingebettet (Embedded), sonst Hidden.
///
/// `cancel`: wird das Flag gesetzt (stop-Request/stdin-EOF), bricht die
/// Verhandlung ab — der Portal-Dialog blockt sonst UNBEGRENZT, und `stop()`
/// joint den Worker: die ganze RPC-Schleife (und der Prozess-Shutdown) hinge
/// fest, bis der User den Dialog beantwortet. Abbruch wird als
/// [`PortalCanceled`] gemeldet (Caller unterscheidet über sein Stop-Flag).
pub fn open(show_cursor: bool, cancel: &AtomicBool) -> Result<PortalSession> {
    portal_runtime().block_on(async move {
        // Auch Proxy-Aufbau + create_session ins select: ein HÄNGENDES (nicht
        // totes) xdg-desktop-portal blockt sonst genau hier ohne je das
        // Cancel-Flag zu sehen — zbus-Calls haben kein Default-Timeout.
        // (Abbruch mitten in create_session kann serverseitig eine Session
        // hinterlassen, die wir nie erfahren — seltener Preis, den Hänger der
        // ganzen RPC-Schleife ist teurer.)
        let sc: Screencast<'static> = tokio::select! {
            r = Screencast::new() => r.map_err(|e| anyhow!("Screencast::new: {e}"))?,
            _ = wait_cancel(cancel) => {
                return Err(anyhow!(PortalCanceled).context("Portal-Aufbau abgebrochen (stop)"));
            }
        };
        let session = tokio::select! {
            r = sc.create_session() => r.map_err(|e| anyhow!("create_session: {e}"))?,
            _ = wait_cancel(cancel) => {
                return Err(anyhow!(PortalCanceled).context("Portal-Aufbau abgebrochen (stop)"));
            }
        };

        let negotiated = tokio::select! {
            r = negotiate(&sc, &session, show_cursor) => r,
            _ = wait_cancel(cancel) => {
                Err(anyhow!(PortalCanceled).context("Verhandlung abgebrochen (stop)"))
            }
        };
        match negotiated {
            Ok((pw_fd, node_id, width, height, restore_token)) => {
                // Close-Task: lebt auf dem portal_runtime und wartet, bis der
                // Guard (Sender) im PortalSession-Drop fällt — dann Session
                // explizit schließen, damit der Compositor die Aufnahme-Anzeige
                // beendet. Fehler beim Close sind egal (Session evtl. schon weg).
                let (close_tx, close_rx) = oneshot::channel::<()>();
                tokio::spawn(async move {
                    let _ = close_rx.await;
                    match tokio::time::timeout(Duration::from_secs(5), session.close()).await {
                        Ok(Err(e)) => {
                            tracing::debug!(target: "stream", "portal session close: {e}");
                        }
                        Err(_) => {
                            tracing::debug!(target: "stream", "portal session close: Timeout");
                        }
                        Ok(Ok(())) => {}
                    }
                    drop(sc);
                });
                Ok(PortalSession {
                    pw_fd,
                    node_id,
                    width,
                    height,
                    restore_token,
                    _close_guard: SessionCloseGuard(close_tx),
                })
            }
            Err(e) => {
                // Fehlgeschlagene/abgebrochene Verhandlung: Session sofort
                // schließen, sonst bleibt sie als Leiche im Compositor.
                // Mit Timeout: dieser Pfad läuft auch beim Cancel gegen ein
                // ggf. HÄNGENDES Portal — `stop()` darf hier nicht ewig
                // festhängen.
                let _ = tokio::time::timeout(Duration::from_secs(5), session.close()).await;
                Err(e)
            }
        }
    })
}

/// Pollt das Abbruch-Flag (100-ms-Raster reicht — es geht um Sekunden-Hänger).
async fn wait_cancel(flag: &AtomicBool) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Die eigentliche Verhandlung (SelectSources → Start → OpenPipeWireRemote),
/// getrennt von `open()`, damit der Fehlerpfad dort die Session schließen kann.
/// Liefert (pw_fd, node_id, width, height, restore_token).
async fn negotiate(
    sc: &Screencast<'static>,
    session: &Session<'static, Screencast<'static>>,
    show_cursor: bool,
) -> Result<(OwnedFd, u32, u32, u32, Option<String>)> {
    let cursor = if show_cursor {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    // Monitor ODER Window zur Auswahl anbieten.
    let types = SourceType::Monitor | SourceType::Window;
    sc.select_sources(
        session,
        cursor,
        types,
        false, // multiple — einzelne Quelle
        None,  // restore_token (noch nicht persistiert)
        // Solange wir kein restore_token einlösen, NICHT persistieren:
        // ExplicitlyRevoked ließe das Portal bei jedem Start einen neuen
        // Permission-Eintrag anlegen, der nie wiederverwendet oder revoked
        // wird — die akkumulieren im Portal-Store. Umstellen auf
        // ExplicitlyRevoked, sobald der Settings-Store das Token speichert.
        PersistMode::DoNot,
    )
    .await
    .map_err(|e| anyhow!("select_sources: {e}"))?
    .response()
    .map_err(|e| cancel_or_err("select_sources", e))?;

    let streams = sc
        .start(session, None)
        .await
        .map_err(|e| anyhow!("start: {e}"))?
        .response()
        .map_err(|e| cancel_or_err("start", e))?;

    let fd = sc
        .open_pipe_wire_remote(session)
        .await
        .map_err(|e| anyhow!("open_pipe_wire_remote: {e}"))?;

    let first = streams
        .streams()
        .first()
        .ok_or_else(|| anyhow!("Start lieferte keine Streams (User-Abbruch?)"))?;
    let node_id = first.pipe_wire_node_id();
    // `size` ist im Portal-Protokoll optional; 0×0 läge außerhalb der
    // VideoSize-Range (min 1×1) im EnumFormat-POD und kann die Verhandlung
    // bei strengen Servern scheitern lassen — auf mindestens 1×1 clampen
    // (die verbindliche Größe kommt ohnehin aus der Format-Verhandlung).
    let (w, h) = first.size().map(|(w, h)| (w.max(1), h.max(1))).unwrap_or((1, 1));
    let restore_token = streams.restore_token().map(str::to_string);

    Ok((fd, node_id, w as u32, h as u32, restore_token))
}

/// ashpd meldet einen User-Abbruch als Error — wir wandeln das in einen
/// Exit-60-markierten Fehler um (Caller wertet `is_portal_canceled` aus).
/// Match über die Fehler-TYPEN, nicht über Display-Strings: der frühere
/// `contains("response")`-Match klassifizierte jeden Backend-Fehler mit
/// "response" im Text (z. B. `NoResponse` → "Portal error: no response") als
/// User-Abbruch — der User sah dann gar keinen Fehler.
fn cancel_or_err(step: &str, e: ashpd::Error) -> anyhow::Error {
    use ashpd::desktop::ResponseError;
    let canceled = matches!(
        e,
        ashpd::Error::Response(ResponseError::Cancelled)
            | ashpd::Error::Portal(ashpd::PortalError::Cancelled(_))
    );
    if canceled {
        anyhow!(PortalCanceled).context(format!("{step} abgebrochen"))
    } else {
        anyhow!("{step}: {e}")
    }
}

/// Marker-Fehler für User-Abbruch — Caller beendet mit Exit 60.
#[derive(Debug)]
struct PortalCanceled;

impl std::fmt::Display for PortalCanceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "portal dialog canceled by user")
    }
}
impl std::error::Error for PortalCanceled {}

/// True wenn der Fehler ein User-Abbruch des Portal-Dialogs ist.
pub fn is_portal_canceled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<PortalCanceled>().is_some())
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use ashpd::desktop::ResponseError;

    #[test]
    fn user_cancel_is_marked_as_canceled() {
        let e = cancel_or_err("start", ashpd::Error::Response(ResponseError::Cancelled));
        assert!(is_portal_canceled(&e));
    }

    /// Echte Portal-Fehler dürfen NICHT als User-Abbruch (Exit 60) durchgehen —
    /// sonst sieht der User bei einem kaputten Portal-Backend keinerlei Fehler.
    /// `NoResponse` rendert als "Portal error: no response" und triggert den
    /// alten `contains("response")`-Match fälschlich.
    #[test]
    fn backend_errors_are_not_cancel() {
        let e = cancel_or_err("start", ashpd::Error::NoResponse);
        assert!(!is_portal_canceled(&e), "NoResponse ist ein Backend-Fehler, kein Abbruch: {e:#}");
        let e = cancel_or_err("start", ashpd::Error::Response(ResponseError::Other));
        assert!(!is_portal_canceled(&e), "ResponseError::Other ist kein User-Abbruch: {e:#}");
    }
}
