//! `gpueue-helper`: the root helper (`serve`, the default) and the job
//! unit's shim (`shim`, `shim-init`, internal).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use gpueue_checkpoint::{CudaMode, Executor, SystemExecutor};
use gpueue_config::DEFAULT_CONFIG_PATH;
use gpueue_helper::config::{Overrides, Resolved};
use gpueue_helper::host::SystemHost;
use gpueue_helper::launcher::Launcher;
use gpueue_helper::launcher::systemd::SystemdLauncher;
use gpueue_helper::{Helper, HelperConfig, notify, server, shim};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Debug, Parser)]
#[command(
    name = "gpueue-helper",
    version,
    about = "Root helper of the gpueue job queue",
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
    /// Look up PATH, which makes the automounter mount it (internal, run
    /// by the shim).
    #[command(hide = true)]
    ShimAutomount { path: PathBuf },
    /// Check, as a job's owner, what a restore opens (internal; request
    /// on stdin, see `gpueue_helper::access`).
    #[command(hide = true)]
    VerifyAccess,
    /// CRIU action script of a restore: at `pre-resume`, check the
    /// restored tasks (internal; see `gpueue_helper::access`).
    #[command(hide = true)]
    VerifyRestoreHook { request: PathBuf },
    /// `--action-script` of `criu dump`: records the files of the dumped
    /// job (internal; see `gpueue_helper::access::run_dump_hook`).
    #[command(hide = true)]
    DumpHook { request: PathBuf },
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

/// Flags of `serve`. Every setting except `--config` defaults to the
/// shared configuration file, whose missing keys take the built-in defaults.
#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// The configuration file shared with the daemon. It must be owned by
    /// root and not group/world-writable (and so must its directories).
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    /// [config: helper_socket]
    #[arg(long)]
    socket: Option<PathBuf>,
    /// The only user allowed to connect. [config: daemon_user]
    #[arg(long)]
    daemon_user: Option<String>,
    /// [config: helper_state_dir, default <data_dir>/helper]
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// [config: checkpoints_dir, default <data_dir>/checkpoints]
    #[arg(long)]
    checkpoint_dir: Option<PathBuf>,
    /// [config: logs_dir, default <data_dir>/logs]
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// Bind mount of `/` for `criu restore --root`; empty disables it.
    /// [default: /run/gpueue-helper/criu-root]
    #[arg(long)]
    criu_root: Option<PathBuf>,
    /// `manual`: the helper calls cuda-checkpoint around criu (works with
    /// the distribution's criu). `plugin`: CRIU's CUDA plugin does it.
    /// [config: cuda_checkpoint_mode]
    #[arg(long, value_enum)]
    cuda_mode: Option<ModeArg>,
    /// [config: criu_path]
    #[arg(long)]
    criu: Option<PathBuf>,
    /// [config: cuda_checkpoint_path]
    #[arg(long)]
    cuda_checkpoint: Option<PathBuf>,
    /// [config: nvidia_smi_path]
    #[arg(long)]
    nvidia_smi: Option<PathBuf>,
    /// Seconds systemd waits between SIGTERM and SIGKILL when stopping a job.
    /// [config: grace_secs]
    #[arg(long)]
    job_stop_timeout: Option<u64>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command.unwrap_or(Command::Serve(Box::new(cli.serve))) {
        // The shim must stay single-threaded: no runtime.
        Command::Shim { job_dir } => shim::run_outer(job_dir),
        Command::ShimInit { job_dir } => shim::run_init(job_dir),
        Command::ShimAutomount { path } => shim::run_automount(path),
        Command::CriuWrap { root, criu } => shim::run_criu_wrap(root, criu),
        Command::VerifyAccess => gpueue_helper::access::run_verify_access(),
        Command::VerifyRestoreHook { request } => gpueue_helper::access::run_restore_hook(&request),
        Command::DumpHook { request } => gpueue_helper::access::run_dump_hook(&request),
        Command::Serve(args) => match serve(*args) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("gpueue-helper: {e}");
                1
            }
        },
    };
    ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1))
}

/// `<criu> --version`'s output (see `HelperConfig::check_criu`).
fn criu_version(criu: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new(criu)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Flag > `config.toml` > built-in default.
fn config(args: ServeArgs) -> Result<HelperConfig, String> {
    let file = gpueue_config::Config::load_trusted(&args.config).map_err(|e| e.to_string())?;
    let overrides = Overrides {
        socket: args.socket,
        daemon_user: args.daemon_user,
        state_dir: args.state_dir,
        checkpoint_dir: args.checkpoint_dir,
        log_dir: args.log_dir,
        criu_root: args.criu_root,
        cuda_mode: args.cuda_mode.map(|mode| match mode {
            ModeArg::Plugin => CudaMode::Plugin,
            ModeArg::Manual => CudaMode::Manual,
        }),
        criu: args.criu,
        cuda_checkpoint: args.cuda_checkpoint,
        nvidia_smi: args.nvidia_smi,
        job_stop_timeout: args.job_stop_timeout.map(Duration::from_secs),
    };
    let Resolved {
        config: mut cfg,
        daemon_user,
    } = HelperConfig::resolve(&file, overrides);
    let user = nix::unistd::User::from_name(&daemon_user)
        .map_err(|e| format!("looking up {daemon_user}: {e}"))?
        .ok_or_else(|| format!("user {daemon_user} does not exist"))?;
    if user.uid.is_root() {
        return Err(format!("the daemon user {daemon_user} must not be root"));
    }
    cfg.daemon_uid = user.uid.as_raw();
    cfg.daemon_gid = user.gid.as_raw();
    cfg.helper_exe = std::env::current_exe().map_err(|e| format!("locating myself: {e}"))?;
    cfg.gc_roots = gpueue_helper::gcroots::GcRoots::detect(&cfg.helper_exe);
    if let Some(target) = cfg.gc_roots.target() {
        eprintln!(
            "gpueue-helper: running from the Nix store; jobs keep {} as GC roots in {}/gpueue",
            target.display(),
            gpueue_helper::gcroots::NIX_GCROOTS
        );
    }
    eprintln!(
        "gpueue-helper: config {}; daemon user {daemon_user}; socket {}; checkpoints {}; \
         logs {}; cuda mode {}; criu {}; cuda-checkpoint {}",
        args.config.display(),
        cfg.socket_path.display(),
        cfg.checkpoint_base.display(),
        cfg.log_base.display(),
        match cfg.checkpoint.mode {
            CudaMode::Plugin => "plugin",
            CudaMode::Manual => "manual",
        },
        cfg.checkpoint.criu.criu_path.display(),
        cfg.checkpoint.cuda.path.display(),
    );
    Ok(cfg)
}

#[tokio::main]
async fn serve(args: ServeArgs) -> Result<(), String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("must run as root".into());
    }
    let mut cfg = config(args)?;
    // Programs given by name are looked up once, in the service's PATH,
    // and then checked and used by their absolute path.
    let path_env = std::env::var_os("PATH")
        .unwrap_or_else(|| gpueue_config::trust::DEFAULT_SERVICE_PATH.into());
    for warning in cfg.resolve_programs(&path_env)? {
        eprintln!("gpueue-helper: WARN {warning}");
    }
    eprintln!(
        "gpueue-helper: criu {}; cuda-checkpoint {}; nvidia-smi {}",
        cfg.checkpoint.criu.criu_path.display(),
        cfg.checkpoint.cuda.path.display(),
        cfg.nvidia_smi.display()
    );
    let criu = cfg.check_criu(criu_version);
    eprintln!("gpueue-helper: {criu}");
    eprintln!("gpueue-helper: job policy: {}", cfg.job_policy.describe());
    for path in cfg.paths_to_check() {
        gpueue_helper::validate::check_root_owned_path(path)?;
    }
    // Job units, restores and the access checks run this binary as root.
    gpueue_helper::validate::check_root_owned_path(&cfg.helper_exe)
        .map_err(|e| format!("the helper binary {}: {e}", cfg.helper_exe.display()))?;
    if let Some(dir) = &cfg.fork_guard_dir {
        // Without it jobs still run; only forked children of CUDA
        // processes keep their dumps from succeeding.
        match gpueue_helper::forkguard::install(dir)
            .map_err(|e| e.to_string())
            .and_then(|lib| {
                gpueue_helper::validate::check_root_owned_path(&lib)?;
                Ok(lib)
            }) {
            Ok(lib) => {
                eprintln!("gpueue-helper: fork guard {}", lib.display());
                cfg.job_preload = Some(lib);
            }
            Err(e) => eprintln!(
                "gpueue-helper: WARN cannot install the fork guard in {}: {e}; \
                 jobs that fork CUDA processes cannot be checkpointed",
                dir.display()
            ),
        }
    }
    // Job units run a copy of this binary that upgrades do not replace, so
    // that their checkpoints can still be restored (see
    // `gpueue_helper::helperbin`). Not from the Nix store, whose paths do
    // not change (the GC roots keep them).
    match &cfg.helper_copy_dir {
        Some(dir) if cfg.gc_roots.target().is_none() => {
            match gpueue_helper::helperbin::install(dir, &cfg.helper_exe)
                .map_err(|e| e.to_string())
                .and_then(|copy| {
                    gpueue_helper::validate::check_root_owned_path(&copy)?;
                    Ok(copy)
                }) {
                Ok(copy) => {
                    eprintln!("gpueue-helper: job units run {}", copy.display());
                    cfg.shim_label = gpueue_helper::helperbin::label_of(&cfg.helper_exe);
                    cfg.shim_exe = Some(copy);
                }
                Err(e) => eprintln!(
                    "gpueue-helper: WARN cannot copy {} into {}: {e}; job units run it \
                     directly, and their checkpoints cannot be restored once an upgrade \
                     replaces it",
                    cfg.helper_exe.display(),
                    dir.display()
                ),
            }
        }
        _ => {}
    }
    // Also in plugin mode: an image is restored in the mode it was dumped
    // in. Its ancestors were checked above, so nobody but root can have put
    // anything on the way.
    {
        use std::os::unix::fs::DirBuilderExt;
        let dir = &cfg.checkpoint.criu.empty_plugin_dir;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .map_err(|e| format!("creating the empty CRIU plugin dir: {e}"))?;
        gpueue_helper::validate::check_root_owned_path(dir)?;
        if std::fs::read_dir(dir).map_or(true, |mut d| d.next().is_some()) {
            return Err(format!(
                "{} must be an empty directory (criu loads every plugin in it)",
                dir.display()
            ));
        }
    }
    let launcher = SystemdLauncher::connect(
        cfg.helper_exe.clone(),
        cfg.spawn_timeout,
        cfg.job_stop_timeout,
    )
    .await
    .map_err(|e| format!("connecting to systemd: {e}"))?;
    let mut host = SystemHost::new(
        cfg.nvidia_smi.clone(),
        cfg.checkpoint.cuda.clone(),
        cfg.helper_exe.clone(),
    );
    // Directories of system accounts that no job runs as (such as
    // systemd-resolve's /run/systemd/resolve) may lie on the way to a host
    // path that CRIU binds; jobs' owners cannot change them.
    host.bind_trust =
        gpueue_helper::mountfd::OwnerTrust::system(cfg.job_policy.min_uid, cfg.daemon_uid);
    let factory: gpueue_helper::ExecutorFactory =
        Arc::new(|| Box::new(SystemExecutor::default()) as Box<dyn Executor + Send>);
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
    // re-discovers them. `gpueue shutdown` dumps through the daemon's
    // checkpoint requests instead.
    let stopping = helper
        .launcher()
        .system_stopping()
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "gpueue-helper: cannot tell whether the system is stopping ({e}); assuming it is"
            );
            true
        });
    if !stopping {
        eprintln!(
            "gpueue-helper: stopping without checkpointing (the system is not shutting down)"
        );
        // Let spawns, restores and checkpoints in flight finish or be
        // abandoned (a second signal exits at once); the connections stay
        // open so their answers and events still reach the daemon.
        tokio::select! {
            _ = helper.drain() => {}
            _ = term.recv() => eprintln!("gpueue-helper: second signal, exiting now"),
            _ = int.recv() => eprintln!("gpueue-helper: second signal, exiting now"),
        }
        let _ = stop_tx.send(true);
        let _ = std::fs::remove_file(&socket_path);
        return Ok(());
    }
    eprintln!(
        "gpueue-helper: system shutdown; checkpointing running jobs (signal again to exit now)"
    );
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        eprintln!("gpueue-helper: second signal, exiting immediately");
        std::process::exit(1);
    });

    let manifest = helper
        .shutdown(
            |job| eprintln!("gpueue-helper: checkpointing job {}", job.0),
            notify::extend_timeout,
        )
        .await;
    eprintln!(
        "gpueue-helper: shutdown manifest written ({} jobs)",
        manifest.jobs.len()
    );
    let _ = stop_tx.send(true);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
