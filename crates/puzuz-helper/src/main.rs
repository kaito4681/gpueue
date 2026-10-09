//! `puzuz-helper`: the root helper (`serve`, the default) and the job
//! unit's shim (`shim`, `shim-init`, internal).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use puzuz_checkpoint::{CudaMode, Executor, SystemExecutor};
use puzuz_helper::config::{
    DEFAULT_CHECKPOINT_BASE, DEFAULT_CRIU_ROOT, DEFAULT_LOG_BASE, DEFAULT_STATE_DIR,
};
use puzuz_helper::host::SystemHost;
use puzuz_helper::launcher::Launcher;
use puzuz_helper::launcher::systemd::SystemdLauncher;
use puzuz_helper::{Helper, HelperConfig, notify, server, shim};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Debug, Parser)]
#[command(
    name = "puzuz-helper",
    version,
    about = "Root helper of the puzuz job queue",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Options of `serve` when no subcommand is given.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the helper service (the default).
    Serve(Box<ServeArgs>),
    /// Main process of a job unit (internal).
    #[command(hide = true)]
    Shim { job_dir: PathBuf },
    /// PID 1 of a job's namespace (internal).
    #[command(hide = true)]
    ShimInit { job_dir: PathBuf },
    /// `ExecStart` of a restore unit: bind-mount / on ROOT in a private
    /// mount namespace (unless ROOT is empty), then exec criu (internal).
    #[command(hide = true)]
    CriuWrap {
        root: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        criu: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Plugin,
    Manual,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    #[arg(long, default_value = puzuz_protocol::helper::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,
    /// The only user allowed to connect (the daemon's system user).
    #[arg(long, default_value = "puzuz")]
    daemon_user: String,
    #[arg(long, default_value = DEFAULT_STATE_DIR)]
    state_dir: PathBuf,
    #[arg(long, default_value = DEFAULT_CHECKPOINT_BASE)]
    checkpoint_dir: PathBuf,
    #[arg(long, default_value = DEFAULT_LOG_BASE)]
    log_dir: PathBuf,
    /// Bind mount of `/` for `criu restore --root`; empty disables it.
    #[arg(long, default_value = DEFAULT_CRIU_ROOT)]
    criu_root: PathBuf,
    /// `manual`: the helper calls cuda-checkpoint around criu (works with
    /// the distribution's criu). `plugin`: CRIU's CUDA plugin does it.
    #[arg(long, value_enum, default_value = "manual")]
    cuda_mode: ModeArg,
    #[arg(long, default_value = "criu")]
    criu: PathBuf,
    #[arg(long, default_value = "cuda-checkpoint")]
    cuda_checkpoint: PathBuf,
    #[arg(long, default_value = "nvidia-smi")]
    nvidia_smi: PathBuf,
    /// Seconds systemd waits between SIGTERM and SIGKILL when stopping a job.
    #[arg(long, default_value_t = 30)]
    job_stop_timeout: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command.unwrap_or(Command::Serve(Box::new(cli.serve))) {
        // The shim must stay single-threaded: no runtime.
        Command::Shim { job_dir } => shim::run_outer(job_dir),
        Command::ShimInit { job_dir } => shim::run_init(job_dir),
        Command::CriuWrap { root, criu } => shim::run_criu_wrap(root, criu),
        Command::Serve(args) => match serve(*args) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("puzuz-helper: {e}");
                1
            }
        },
    };
    ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1))
}

fn config(args: ServeArgs) -> Result<HelperConfig, String> {
    let user = nix::unistd::User::from_name(&args.daemon_user)
        .map_err(|e| format!("looking up {}: {e}", args.daemon_user))?
        .ok_or_else(|| format!("user {} does not exist", args.daemon_user))?;
    let mut cfg = HelperConfig {
        socket_path: args.socket,
        daemon_uid: user.uid.as_raw(),
        daemon_gid: user.gid.as_raw(),
        state_dir: args.state_dir,
        checkpoint_base: args.checkpoint_dir,
        log_base: args.log_dir,
        criu_root: (!args.criu_root.as_os_str().is_empty()).then_some(args.criu_root),
        nvidia_smi: args.nvidia_smi,
        job_stop_timeout: Duration::from_secs(args.job_stop_timeout),
        helper_exe: std::env::current_exe().map_err(|e| format!("locating myself: {e}"))?,
        ..HelperConfig::default()
    };
    cfg.checkpoint.mode = match args.cuda_mode {
        ModeArg::Plugin => CudaMode::Plugin,
        ModeArg::Manual => CudaMode::Manual,
    };
    cfg.checkpoint.criu.criu_path = args.criu;
    cfg.checkpoint.cuda.path = args.cuda_checkpoint;
    Ok(cfg)
}

#[tokio::main]
async fn serve(args: ServeArgs) -> Result<(), String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("must run as root".into());
    }
    let cfg = config(args)?;
    let mut must_be_root_owned = vec![&cfg.state_dir, &cfg.checkpoint_base, &cfg.log_base];
    if let Some(root) = &cfg.criu_root {
        must_be_root_owned.push(root);
    }
    for dir in must_be_root_owned {
        puzuz_helper::validate::check_root_owned_path(dir)?;
    }
    if cfg.checkpoint.mode == CudaMode::Manual {
        std::fs::create_dir_all(&cfg.checkpoint.criu.empty_plugin_dir)
            .map_err(|e| format!("creating the empty CRIU plugin dir: {e}"))?;
    }
    let launcher = SystemdLauncher::connect(
        cfg.helper_exe.clone(),
        cfg.spawn_timeout,
        cfg.job_stop_timeout,
    )
    .await
    .map_err(|e| format!("connecting to systemd: {e}"))?;
    let host = SystemHost::new(cfg.nvidia_smi.clone(), cfg.checkpoint.cuda.clone());
    let factory: puzuz_helper::ExecutorFactory =
        Arc::new(|| Box::new(SystemExecutor::default()) as Box<dyn Executor + Send>);
    let dump_budget = cfg.checkpoint.timeouts.criu_dump + cfg.checkpoint.timeouts.cuda_checkpoint;
    let socket_path = cfg.socket_path.clone();
    let daemon_uid = cfg.daemon_uid;

    let helper = Helper::new(cfg, launcher, host, factory).map_err(|e| format!("state: {e}"))?;
    helper
        .recover()
        .await
        .map_err(|e| format!("recovering jobs: {e}"))?;
    let listener = server::bind(&socket_path, daemon_uid)
        .map_err(|e| format!("binding {}: {e}", socket_path.display()))?;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(server::serve(helper.clone(), listener, stop_rx.clone()));
    tokio::spawn(helper.clone().monitor(stop_rx));
    let _ = notify::notify("READY=1");

    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    let _ = notify::notify("STOPPING=1");
    // Only an OS shutdown/reboot dumps the jobs. A restart or stop of the
    // helper alone (e.g. an upgrade) leaves them running; the next helper
    // re-discovers them. `puzuz shutdown` dumps through the daemon's
    // checkpoint requests instead.
    let stopping = helper
        .launcher()
        .system_stopping()
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "puzuz-helper: cannot tell whether the system is stopping ({e}); assuming it is"
            );
            true
        });
    if !stopping {
        eprintln!("puzuz-helper: stopping without checkpointing (the system is not shutting down)");
        let _ = stop_tx.send(true);
        let _ = std::fs::remove_file(&socket_path);
        return Ok(());
    }
    eprintln!(
        "puzuz-helper: system shutdown; checkpointing running jobs (signal again to exit now)"
    );
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        eprintln!("puzuz-helper: second signal, exiting immediately");
        std::process::exit(1);
    });

    let manifest = helper
        .shutdown(|job| {
            eprintln!("puzuz-helper: checkpointing job {}", job.0);
            notify::extend_timeout(dump_budget);
        })
        .await;
    eprintln!(
        "puzuz-helper: shutdown manifest written ({} jobs)",
        manifest.jobs.len()
    );
    let _ = stop_tx.send(true);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
