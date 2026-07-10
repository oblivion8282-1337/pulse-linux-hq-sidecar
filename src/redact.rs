//! Token-Redaction für Logs & Diagnose-Ausgaben.
//!
//! Die Push-URL trägt das Stream-Token (`pass=`/`token=`/`streamid=publish:`).
//! Es darf NIE roh in Logs, argv-Diagnostik oder Events erscheinen — weder auf
//! stderr (Pulse tee't stderr in `sidecar.log`, ggf. hochgeladen) noch im
//! Protokoll. Spiegelt `streaming/gsr-sidecar/redact.py`.

/// Ersetzt den Wert von `pass=`/`token=`/`streamid=publish:` durch `***`.
/// Idempotent und allocation-arm (nur bei Treffer eine Kopie).
pub fn redact_url(url: &str) -> String {
    let mut s = url.to_string();
    for pat in ["pass=", "token=", "streamid=publish:"] {
        if let Some(idx) = s.find(pat) {
            let start = idx + pat.len();
            let end = s[start..]
                .find(|c: char| c == '&' || c == ' ')
                .map(|i| start + i)
                .unwrap_or(s.len());
            s.replace_range(start..end, "***");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redacts_pass_and_token() {
        assert_eq!(
            redact_url("rtmps://h:1936/app?user=pulse&pass=SECRET"),
            "rtmps://h:1936/app?user=pulse&pass=***"
        );
        assert_eq!(redact_url("rtmp://h/live?token=ABC&x=1"), "rtmp://h/live?token=***&x=1");
    }

    #[test]
    fn leaves_clean_urls_untouched() {
        let u = "rtmps://localhost:11936/live/test";
        assert_eq!(redact_url(u), u);
    }
}
