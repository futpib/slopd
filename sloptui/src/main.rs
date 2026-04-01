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

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let _log_guard = libsloptui_dioxus::setup_logging(cli.verbose);

    libsloptui_dioxus::launch_unix();
}
