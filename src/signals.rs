//! Signal names for `tui_signal` and exit-status messages.
//!
//! Signal numbers differ between Linux and macOS (e.g. `SIGUSR1` is 10 on Linux and 30 on
//! macOS), so names are mapped through `rustix`'s constants for the platform we run on.

pub use rustix::process::Signal;

/// Signals `tui_signal` can send, by their name without the `SIG` prefix.
const SIGNALS: [(&str, Signal); 20] = [
    ("HUP", Signal::HUP),
    ("INT", Signal::INT),
    ("QUIT", Signal::QUIT),
    ("TRAP", Signal::TRAP),
    ("ABRT", Signal::ABORT),
    ("BUS", Signal::BUS),
    ("FPE", Signal::FPE),
    ("KILL", Signal::KILL),
    ("USR1", Signal::USR1),
    ("USR2", Signal::USR2),
    ("SEGV", Signal::SEGV),
    ("PIPE", Signal::PIPE),
    ("ALRM", Signal::ALARM),
    ("TERM", Signal::TERM),
    ("CONT", Signal::CONT),
    ("STOP", Signal::STOP),
    ("TSTP", Signal::TSTP),
    ("TTIN", Signal::TTIN),
    ("TTOU", Signal::TTOU),
    ("WINCH", Signal::WINCH),
];

/// Parses a signal name such as `"INT"`, `"SIGINT"` or `"int"`.
pub fn parse_signal(name: &str) -> anyhow::Result<Signal> {
    let upper = name.trim().to_ascii_uppercase();
    let short = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS
        .iter()
        .find(|(known, _)| *known == short)
        .map(|(_, signal)| *signal)
        .ok_or_else(|| {
            let names: Vec<&str> = SIGNALS.iter().map(|(known, _)| *known).collect();
            anyhow::anyhow!("unknown signal '{name}'; use one of {}", names.join(", "))
        })
}

/// The `SIG…` name of a signal number on this platform, if it's one we know.
#[must_use]
pub fn signal_name(signal: i32) -> Option<String> {
    SIGNALS
        .iter()
        .find(|(_, known)| known.as_raw() == signal)
        .map(|(name, _)| format!("SIG{name}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_signal_accepts_short_long_and_lowercase_names() {
        assert_eq!(parse_signal("INT").unwrap(), Signal::INT);
        assert_eq!(parse_signal("SIGTERM").unwrap(), Signal::TERM);
        assert_eq!(parse_signal(" tstp ").unwrap(), Signal::TSTP);
        assert_eq!(parse_signal("sigusr1").unwrap(), Signal::USR1);
        assert_eq!(parse_signal("TRAP").unwrap(), Signal::TRAP);
        assert_eq!(parse_signal("SIGBUS").unwrap(), Signal::BUS);
        assert_eq!(parse_signal("fpe").unwrap(), Signal::FPE);
    }

    #[test]
    fn test_parse_signal_rejects_unknown_names() {
        let err = parse_signal("SIGNOPE").unwrap_err().to_string();
        assert!(err.contains("unknown signal 'SIGNOPE'"), "{err}");
        assert!(err.contains("INT, QUIT, TRAP, ABRT, BUS, FPE"), "{err}");
        assert!(parse_signal("9").is_err());
    }

    #[test]
    fn test_signal_name_uses_platform_numbers() {
        assert_eq!(signal_name(9).as_deref(), Some("SIGKILL"));
        assert_eq!(
            signal_name(Signal::USR1.as_raw()).as_deref(),
            Some("SIGUSR1")
        );
        assert_eq!(signal_name(0), None);
    }
}
