//! The process-wide `tracing` subscriber.

use std::sync::OnceLock;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// Enables the stderr sink when truthy (`1`, `true`, `yes`, `on`).
pub const ENV_STDERR: &str = "ASTER_LOG_STDERR";
/// `EnvFilter` directives for the stderr sink.
pub const ENV_FILTER: &str = "ASTER_LOG";
/// The filter when [`ENV_FILTER`] is unset or empty.
pub const DEFAULT_FILTER: &str = "warn,aster=info";

/// What [`init_stderr_from_env`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrInit {
    /// The stderr sink is the global subscriber.
    Installed,
    /// [`ENV_STDERR`] is not set: nothing was installed and events cost what
    /// they cost with no subscriber.
    Disabled,
    /// Another global subscriber was already installed (a Rust application
    /// embedding Aster); it is left alone.
    Foreign,
}

static STDERR: OnceLock<StderrInit> = OnceLock::new();

/// Install the stderr sink as the global subscriber if [`ENV_STDERR`] asks for
/// it. Idempotent: the environment is read on the first call only, and every
/// later call returns the first call's outcome.
///
/// An invalid [`ENV_FILTER`] falls back to [`DEFAULT_FILTER`] with a note on
/// stderr. When installed, one `INFO` event on target `aster::otel` records the
/// filter in effect.
pub fn init_stderr_from_env() -> StderrInit {
    *STDERR.get_or_init(|| {
        if !truthy(std::env::var(ENV_STDERR).ok().as_deref()) {
            return StderrInit::Disabled;
        }
        let directives = std::env::var(ENV_FILTER)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_FILTER.to_owned());
        let filter = EnvFilter::try_new(&directives).unwrap_or_else(|e| {
            eprintln!("aster: ignoring invalid {ENV_FILTER}={directives:?} ({e}); using {DEFAULT_FILTER:?}");
            EnvFilter::new(DEFAULT_FILTER)
        });
        let effective = filter.to_string();
        let stderr = fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_target(true)
            .with_filter(filter);
        match tracing_subscriber::registry().with(stderr).try_init() {
            Ok(()) => {
                tracing::info!(target: "aster::otel", filter = %effective, "stderr logging enabled");
                StderrInit::Installed
            }
            Err(_) => StderrInit::Foreign,
        }
    })
}

fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::truthy;

    #[test]
    fn truthy_accepts_the_usual_spellings_only() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(truthy(Some(v)), "{v:?}");
        }
        for v in ["", "0", "false", "no", "off", "2", "enabled"] {
            assert!(!truthy(Some(v)), "{v:?}");
        }
        assert!(!truthy(None));
    }
}
