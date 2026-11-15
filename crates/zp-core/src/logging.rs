//! Logging setup shared by the GUI and CLI binaries.

use tracing_subscriber::EnvFilter;

/// Default filter used when `RUST_LOG` is not set.
const DEFAULT_FILTER: &str = "info,zp=debug";

/// Initializes `tracing` output to stderr.
///
/// The filter is read from `RUST_LOG`, falling back to [`DEFAULT_FILTER`]. Calling this
/// more than once is harmless; later calls are ignored.
pub fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
