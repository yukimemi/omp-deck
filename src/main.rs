use clap::{Parser, Subcommand};
use omp_deck::bind::{choose_bind, tailscale_ip_output};
use omp_deck::config::Config;
use omp_deck::omp::{Omp, RealOmp};
use omp_deck::repos;
use omp_deck::server::Launcher;
use omp_deck::update::{self, RealUpdater};
use omp_deck::{notify, now_ms, server, view};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const REPO_SCAN_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const REMOTE_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const REMOTE_FAIL_TTL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Parser)]
#[command(
    version,
    about = "Dashboard for the live omp collab sessions on this machine"
)]
struct Cli {
    /// Path to the omp executable (default: look it up on PATH)
    #[arg(long, global = true, env = "OMP_DECK_OMP")]
    omp: Option<PathBuf>,
    /// Config file with [repos] roots and [models] list (default: <config dir>/omp-deck/config.toml)
    #[arg(long, global = true, env = "OMP_DECK_CONFIG")]
    config: Option<PathBuf>,
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
    /// Internal: own the pty of one omp session (spawned by `serve`)
    #[command(hide = true)]
    PtyHost {
        /// The omp executable followed by its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Take over a headless session from this terminal: stop it, then run
    /// `omp --resume` in the foreground
    Resume {
        /// Instance id, session id or a unique prefix of either (default: pick from a list)
        id: Option<String>,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The pty host must not start a runtime, the update check or anything
    // else the server does: it is a tiny process that outlives the server.
    if let Command::PtyHost { argv } = cli.command {
        return omp_deck::omp::run_pty_host(argv);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run(cli))
}

async fn run(cli: Cli) -> ExitCode {
    // Skip the background check for `self-update` itself: it already does
    // its own explicit check, and the two would race the same GitHub call.
    // Also skipped for `resume`: a banner would mix into the foreground TUI.
    let auto_update = if matches!(
        cli.command,
        Command::SelfUpdate { .. } | Command::Resume { .. }
    ) {
        None
    } else {
        update::maybe_spawn_auto_update_check()
    };
    let omp_path = cli.omp.clone();
    let omp = Arc::new(RealOmp::new(cli.omp));
    let result = match cli.command {
        Command::Serve {
            bind,
            discord_webhook,
        } => serve(omp, omp_path, bind, discord_webhook, cli.config).await,
        Command::List { json } => list(&omp, json).await,
        Command::Resume { id } => {
            return match omp_deck::takeover::run(&omp, id.as_deref()).await {
                Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                Err(msg) => {
                    eprintln!("omp-deck: {msg}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::SelfUpdate { yes, check } => update::run_self_update(yes, check).await,
        Command::PtyHost { .. } => unreachable!("handled before the runtime starts"),
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
    omp_path: Option<PathBuf>,
    bind: Option<String>,
    discord_webhook: Option<String>,
    config_path: Option<PathBuf>,
) -> Result<(), String> {
    // Captured before anything runs: a self-update replaces the on-disk
    // binary while this process keeps running, and on Linux that turns
    // `current_exe()` into a `(deleted)`-suffixed path once the original
    // inode is gone.
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let config = Config::load_or_default(config_path.as_deref()).map_err(|e| format!("{e:#}"))?;
    let launcher = Arc::new(Launcher {
        repos: repos::Cache::new(config.repos.roots, REPO_SCAN_TTL),
        remote: omp_deck::remote::Remote::new(
            Arc::new(omp_deck::remote::RealGitHub),
            REMOTE_TTL,
            REMOTE_FAIL_TTL,
        ),
        models: config.models.list,
        sessions_root: omp_deck::sessions::default_root(),
        home: dirs::home_dir(),
    });
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
    if let Some(webhook) = &discord_webhook {
        tokio::spawn(notify::run(omp.clone(), webhook.clone()));
    }
    let (restart_tx, mut restart_rx) = tokio::sync::watch::channel(false);
    let router =
        server::router_with_updater(omp, launcher, Arc::new(RealUpdater::new()), restart_tx);
    // `/api/self-update` (server.rs) flips `restart_tx` once it has replaced
    // the on-disk binary; only then does the graceful shutdown below let
    // `axum::serve` return, which drops the `TcpListener` and frees the
    // port. Only after that does the successor get spawned -- spawning it
    // any earlier would race it for the port (see `update::spawn_successor`).
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = restart_rx.wait_for(|v| *v).await;
        })
        .await
        .map_err(|e| e.to_string())?;
    update::spawn_successor(
        &exe,
        local,
        omp_path.as_deref(),
        config_path.as_deref(),
        discord_webhook.as_deref(),
    )
    .map_err(|e| format!("failed to spawn successor after self-update: {e}"))
}
