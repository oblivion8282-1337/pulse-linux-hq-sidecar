//! Token-Redaction für Logs & Diagnose-Ausgaben.
//!
//! Die Push-URL trägt das Stream-Token (`pass=`/`token=`/`streamid=publish:`).
//! Es darf NIE roh in Logs, argv-Diagnostik oder Events erscheinen — weder auf
//! stderr (Pulse tee't stderr in `sidecar.log`, ggf. hochgeladen) noch im
//! Protokoll. Spiegelt `streaming/gsr-sidecar/redact.py`.

/// Ersetzt den Wert von `pass=`/`token=`/`streamid=publish:` durch `***` —
/// JEDES Vorkommen (zusammengesetzte Diagnose-Strings tragen mehrere URLs)
/// und case-insensitiv. Idempotent.
pub fn redact_url(url: &str) -> String {
    let mut s = url.to_string();
    for pat in ["pass=", "token=", "streamid=publish:"] {
        let mut search_from = 0;
        // Suche auf der lowercase-Kopie, Ersetzung im Original — ASCII-lower
        // erhält Byte-Offsets. Nach jeder Ersetzung hinter dem `***` weiter.
        while let Some(rel) = s[search_from..].to_ascii_lowercase().find(pat) {
            let start = search_from + rel + pat.len();
            let end = s[start..]
                .find(|c: char| c == '&' || c == ' ')
                .map(|i| start + i)
                .unwrap_or(s.len());
            s.replace_range(start..end, "***");
            search_from = start + 3;
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

    #[test]
    fn redacts_every_occurrence_not_just_the_first() {
        // Zusammengesetzte Diagnose-Strings ("von X nach Y") tragen mehrere
        // URLs — jedes Vorkommen muss redigiert werden, nicht nur das erste.
        assert_eq!(
            redact_url("von rtmps://a/x?pass=AAA nach rtmps://b/y?pass=BBB"),
            "von rtmps://a/x?pass=*** nach rtmps://b/y?pass=***"
        );
        assert_eq!(
            redact_url("rtmp://h/live?token=T1&pass=P1&token=T2"),
            "rtmp://h/live?token=***&pass=***&token=***"
        );
    }

    #[test]
    fn redacts_case_insensitively() {
        assert_eq!(
            redact_url("rtmps://h:1936/app?user=pulse&PASS=SECRET"),
            "rtmps://h:1936/app?user=pulse&PASS=***"
        );
        assert_eq!(redact_url("rtmp://h/live?Token=ABC&x=1"), "rtmp://h/live?Token=***&x=1");
    }
}
