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

use anyhow::{Result, anyhow};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use tokio::runtime::Runtime;

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
}

/// Verhandle eine ScreenCast-Session. Öffnet den Portal-Dialog (User wählt
/// Quelle). `show_cursor=true` → Cursor eingebettet (Embedded), sonst Hidden.
pub fn open(show_cursor: bool) -> Result<PortalSession> {
    portal_runtime().block_on(async move {
        let sc = Screencast::new()
            .await
            .map_err(|e| anyhow!("Screencast::new: {e}"))?;
        let session = sc
            .create_session()
            .await
            .map_err(|e| anyhow!("create_session: {e}"))?;

        let cursor = if show_cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        // Monitor ODER Window zur Auswahl anbieten.
        let types = SourceType::Monitor | SourceType::Window;
        sc.select_sources(
            &session,
            cursor,
            types,
            false, // multiple — einzelne Quelle
            None,  // restore_token (noch nicht persistiert)
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(|e| anyhow!("select_sources: {e}"))?
        .response()
        .map_err(|e| cancel_or_err("select_sources", e))?;

        let streams = sc
            .start(&session, None)
            .await
            .map_err(|e| anyhow!("start: {e}"))?
            .response()
            .map_err(|e| cancel_or_err("start", e))?;

        let fd = sc
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|e| anyhow!("open_pipe_wire_remote: {e}"))?;

        let first = streams
            .streams()
            .first()
            .ok_or_else(|| anyhow!("Start lieferte keine Streams (User-Abbruch?)"))?;
        let node_id = first.pipe_wire_node_id();
        let (w, h) = first.size().unwrap_or((0, 0));
        let restore_token = streams.restore_token().map(str::to_string);

        Ok(PortalSession {
            pw_fd: fd,
            node_id,
            width: w as u32,
            height: h as u32,
            restore_token,
        })
    })
}

/// ashpd meldet einen User-Abbruch als Error — wir wandeln das in einen
/// Exit-60-markierten Fehler um (Caller wertet `is_portal_canceled` aus).
fn cancel_or_err(step: &str, e: ashpd::Error) -> anyhow::Error {
    let msg = format!("{e}");
    if msg.to_ascii_lowercase().contains("cancelled")
        || msg.to_ascii_lowercase().contains("canceled")
        || msg.contains("response")
    {
        anyhow!(PortalCanceled).context(format!("{step} abgebrochen"))
    } else {
        anyhow!("{step}: {msg}")
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
