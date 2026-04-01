use tokio::net::UnixStream;
use tracing::debug;

#[derive(clap::Parser)]
#[command(
    name = "sloptui",
    about = "TUI process viewer for slopd",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")
)]
struct Cli {
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        help = "Increase log verbosity (-v INFO, -vv DEBUG, -vvv TRACE)"
    )]
    verbose: u8,
}

#[tokio::main]
async fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let _log_guard = libsloptui_ratatui::setup_logging(cli.verbose);

    let socket_path = libslop::socket_path();
    debug!("connecting to {}", socket_path.display());

    let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {}", socket_path.display(), e);
        std::process::exit(1);
    });
    let (reader, writer) = stream.into_split();
    let mut client = libslopctl::Client::new(reader, writer);

    libsloptui_ratatui::run(&mut client).await.unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });
}
