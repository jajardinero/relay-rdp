mod app;
mod gpu;
mod session;
mod store;
mod ui;
mod util;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Display name. The binary, config directory and Wayland app id use [`APP_ID`].
pub const APP_NAME: &str = "Relay";
pub const APP_ID: &str = "relay";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "A fast, reliable remote desktop client",
    after_help = "Set RELAY_PASSWORD to sign in without being asked (for scripts)."
)]
struct Cli {
    /// Host to connect to (`host` or `host:port`), or a path to a `.rdp` file.
    target: Option<String>,

    /// User name for the connection, e.g. `jdoe` or `CORP\\jdoe`.
    #[arg(short, long)]
    user: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let password = std::env::var("RELAY_PASSWORD")
        .ok()
        .map(zeroize::Zeroizing::new);
    // Don't leak the password to child processes.
    // SAFETY: still single-threaded here; the logging thread starts below.
    unsafe { std::env::remove_var("RELAY_PASSWORD") };

    let _log_guard = init_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting {APP_NAME}");

    // Pin the TLS crypto backend. If a dependency ever enables a second one, rustls can no
    // longer pick automatically and would panic on the first connection.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let launch = cli.target.map(|target| ui::LaunchRequest {
        target,
        user: cli.user,
        password,
    });
    app::run(launch)
}

/// Logs go to stderr and to a daily-rotated file under the XDG state directory.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,relay=debug,wgpu_core=warn,wgpu_hal=warn,naga=warn")
    });

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let (file_layer, guard) = match store::paths::log_dir() {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => {
            let appender = tracing_appender::rolling::daily(dir, "relay.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer);
            (Some(layer), Some(guard))
        }
        _ => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}
