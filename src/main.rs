use clap::{Parser, Subcommand};
use omp_deck::bind::{choose_bind, tailscale_ip_output};
use omp_deck::omp::{Omp, RealOmp};
use omp_deck::{notify, now_ms, server, view};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

mod update;

#[derive(Parser)]
#[command(
    version,
    about = "Dashboard for the live omp collab sessions on this machine"
)]
struct Cli {
    /// Path to the omp executable (default: look it up on PATH)
    #[arg(long, global = true, env = "OMP_DECK_OMP")]
    omp: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the dashboard over HTTP (no authentication: the tailnet is the boundary)
    Serve {
        /// Address to listen on, ADDR:PORT (default: this machine's Tailscale IPv4, any free port)
        #[arg(long)]
        bind: Option<String>,
        /// Discord webhook URL; posts a message once a session gets a title
        #[arg(long, env = "OMP_DECK_DISCORD_WEBHOOK")]
        discord_webhook: Option<String>,
    },
    /// Print the live sessions on the terminal
    List {
        /// Print the parsed model as JSON
        #[arg(long)]
        json: bool,
    },
    /// Check for and install a newer omp-deck release
    SelfUpdate {
        /// Install without prompting
        #[arg(short = 'y', long)]
        yes: bool,
        /// Only check for an update; do not install
        #[arg(long)]
        check: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Skip the background check for `self-update` itself: it already does
    // its own explicit check, and the two would race the same GitHub call.
    let auto_update = if matches!(cli.command, Command::SelfUpdate { .. }) {
        None
    } else {
        update::maybe_spawn_auto_update_check()
    };
    let omp = Arc::new(RealOmp::new(cli.omp));
    let result = match cli.command {
        Command::Serve {
            bind,
            discord_webhook,
        } => serve(omp, bind, discord_webhook).await,
        Command::List { json } => list(&omp, json).await,
        Command::SelfUpdate { yes, check } => update::run_self_update(yes, check).await,
    };
    if let Some(handle) = auto_update {
        update::finalize_auto_update_check(handle).await;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("omp-deck: {msg}");
            ExitCode::FAILURE
        }
    }
}

async fn list(omp: &RealOmp, json: bool) -> Result<(), String> {
    let hosts = omp.list().await.map_err(|e| e.to_string())?;
    if json {
        let out = serde_json::to_string_pretty(&hosts).map_err(|e| e.to_string())?;
        println!("{out}");
    } else {
        print!("{}", view::render_table(&hosts, now_ms()));
    }
    Ok(())
}

async fn serve(
    omp: Arc<RealOmp>,
    bind: Option<String>,
    discord_webhook: Option<String>,
) -> Result<(), String> {
    let tailscale = if bind.is_none() {
        tailscale_ip_output().await
    } else {
        None
    };
    let chosen = choose_bind(bind.as_deref(), tailscale.as_deref())?;
    if let Some(warning) = &chosen.warning {
        eprintln!("omp-deck: {warning}");
    }
    let listener = tokio::net::TcpListener::bind(chosen.addr)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", chosen.addr))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    println!("http://{local}/");
    if let Some(webhook) = discord_webhook {
        tokio::spawn(notify::run(omp.clone(), webhook));
    }
    axum::serve(listener, server::router(omp))
        .await
        .map_err(|e| e.to_string())
}
