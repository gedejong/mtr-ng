use anyhow::{bail, Context};
use clap::{CommandFactory, Parser, ValueEnum};
use mtr_ng::{
    args::ProbeProtocol, report::run_report, ui::run_interactive, Args, MtrSession, Result,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    args.report |= args.json || args.csv;
    if let Err(message) = args.validate() {
        Args::command()
            .error(clap::error::ErrorKind::ValueValidation, message)
            .exit();
    }
    init_logging(&args)?;

    let mode = if args.report { "report" } else { "interactive" };
    info!(
        "Starting mtr-ng v{} ({mode} mode)",
        env!("CARGO_PKG_VERSION")
    );
    info!("Target: {}", args.target);

    if args.protocol != ProbeProtocol::Icmp {
        let name = args.protocol.to_possible_value().expect("not skipped");
        bail!(
            "--protocol {} is not implemented yet; only icmp is supported",
            name.get_name()
        );
    }

    let session = MtrSession::new(args).await?;

    if session.args.report {
        run_report(session).await
    } else {
        run_interactive(session).await
    }
}

/// Report mode logs warnings to stderr. The TUI owns the terminal, so interactive
/// mode only logs with --log-file. RUST_LOG overrides the level in both.
fn init_logging(args: &Args) -> Result<()> {
    let filter = |default: &str| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
    };

    if let Some(path) = &args.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open log file {}", path.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter("mtr_ng=debug"))
            .with_ansi(false)
            .with_writer(file)
            .init();
    } else if args.report {
        tracing_subscriber::fmt()
            .with_env_filter(filter(if args.quiet {
                "mtr_ng=error"
            } else {
                "mtr_ng=warn"
            }))
            .with_writer(std::io::stderr)
            .init();
    }
    Ok(())
}
