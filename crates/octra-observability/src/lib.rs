//! Observability bootstrap shared by the daemon and future binaries.

use octra_config::LogFormat;
use octra_core::{Error, Result};
use tracing_subscriber::EnvFilter;

pub fn init(level: &str, format: LogFormat) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    let initialized = match format {
        LogFormat::Json => builder.json().try_init(),
        LogFormat::Pretty => builder.try_init(),
    };

    initialized.map_err(|err| Error::Config(format!("cannot initialize tracing: {err}")))
}
