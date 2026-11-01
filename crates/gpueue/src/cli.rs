//! Command line interface definition.
//!
//! The subcommands, flags, short flags, aliases and defaults mirror pueue 4.0.x so that existing
//! pueue muscle memory and scripts keep working. Differences from pueue are documented on the
//! respective items and summarized here:
//!
//! - Global options: pueue's `-c/--config` and `-p/--profile` are not offered. gpueue talks to a
//!   single system-wide daemon whose configuration lives in `/etc/gpueue/config.toml`, read by the
//!   daemon, the helper, `doctor` and the CLI's update settings. Instead, `--socket` (or `GPUEUE_SOCKET`) overrides the daemon socket.
//! - `send` is parsed but always fails: jobs run with stdin connected to `/dev/null`.
//! - `edit` and `env` are parsed but not implemented yet.
//! - `shutdown` checkpoints all jobs before stopping (`--no-checkpoint` kills them instead).
//! - `status` accepts pueue's query language plus the gpueue columns `owner` and `gpu` and the
//!   checkpoint states (see `crate::query`). `status --json` prints gpueue's own schema
//!   (`{"groups": [...], "jobs": [...]}`, job views without their environment), not pueue's
//!   state dump.
//! - gpueue extensions: `add --gpu/--no-checkpoint/--nice`, `checkpoint`, `restore`, `gpu`,
//!   `prio`, `nice`, `top`, `site`, `share`, `group priority`, `daemon`, `service`, `uninstall`,
//!   `doctor` and `update`.
//! - Priority: with the default multifactor priority (see `gpueue prio`), `add -o/--priority`
//!   and `switch` only order the caller's own jobs; another user's job is never overtaken
//!   because of them. `priority_type = "basic"` restores pueue's global order.

use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Local, TimeDelta};
use clap::{ArgAction, Parser, Subcommand, ValueEnum, ValueHint};

/// Default path of the daemon's client socket.
pub const DEFAULT_SOCKET: &str = "/run/gpueue/gpueue.sock";

const DELAY_FORMAT_HELP: &str = "DELAY FORMAT:

    The --delay argument is either a number of seconds or a date expression similar to GNU \
    \"date -d\" (the same format pueue accepts). Examples:

    2020-04-01T18:30:00   // RFC 3339 timestamp
    2020-4-1 18:2:30      // Optional leading zeros
    2020-4-1 5:30pm       // Informal am/pm time
    2020-4-1 5pm          // Optional minutes and seconds
    April 1, 2020 18:30   // English months
    Apr 1 8:30pm          // Implies current year
    4/1                   // American form date
    wednesday 10:30pm     // The closest wednesday in the future at 22:30
    wednesday             // The closest wednesday in the future
    4 months              // 4 months from today at 00:00:00
    1 week                // 1 week at the current time
    1days                 // 1 day from today at the current time
    1d 03:00              // The closest 3:00 after 1 day (24 hours)
    3h                    // 3 hours from now
    3600s                 // 3600 seconds from now
";

const STATUS_QUERY_HELP: &str =
    "Filter, order or limit the listed jobs with a small query language \
(the same syntax as pueue).

Syntax:
   [column_selection]? [filter]* [order_by]? [limit]?

where:
  - column_selection := `columns=[column](,[column])*`
  - column := `id | status | priority | command | label | path | enqueue_at | dependencies |
    owner | gpu | start | end`
  - filter := `[filter_column] [filter_op] [filter_value]`
  - filter_column := `status | command | label | start | end | enqueue_at`
  - filter_op := `= | != | < | > | %=`   (`%=` means \"contains\")
  - order_by := `order_by [column] [asc | desc]`
  - limit := `(first | last) [count]`

Filter columns:
  - `status` supports `=` and `!=` with `queued`, `stashed`, `paused`, `running`, `success`,
    `failed`, `done`, `checkpointing`, `checkpointed`, `restore_queued`, `restoring` and
    `terminating`
  - `command` and `label` support `=`, `!=` and `%=`; the value is the rest of the query
  - `start`, `end` and `enqueue_at` support `=`, `!=`, `<` and `>` with `YYYY-MM-DD`,
    `HH:mm[:ss]` or `YYYY-MM-DD[ |T]HH:mm[:ss]`

Examples:
  - `status=running`
  - `command%=echo`
  - `label=nightly`
  - `columns=id,status,command status=running start > 2023-05-21 12:03:17 order_by command first 5`";

/// Top-level command line arguments.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(
    name = "gpueue",
    about = concat!(
        "Interact with the gpueue GPU job queue (a pueue-compatible CLI)\n\n",
        "Use the `--help` long form to get detailed help output on each subcommand!"
    ),
    version
)]
pub struct Cli {
    /// Verbose mode (-v, -vv, -vvv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Colorize the output; `auto` enables colors when stdout is a terminal.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    /// Path of the daemon's Unix socket.
    ///
    /// Clients connect to it and `gpueue daemon` listens on it.
    /// Replaces pueue's `--config`/`--profile`, which do not apply to a system-wide daemon.
    #[arg(
        long,
        env = "GPUEUE_SOCKET",
        value_hint = ValueHint::FilePath,
        global = true,
        default_value = DEFAULT_SOCKET
    )]
    pub socket: PathBuf,

    /// Print JSON: alone, the job list as `status --json` does; before a command that has
    /// `--json`, the same as giving it to that command.
    #[arg(short, long)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Option<SubCommand>,
}

impl Cli {
    /// The subcommand to run. Like pueue, no subcommand means `status` without arguments.
    /// `--json` is passed on to it (see [`Cli::json_unsupported_by`]).
    pub fn subcommand(&self) -> SubCommand {
        let mut cmd = self.cmd.clone().unwrap_or_else(SubCommand::default_status);
        if self.json
            && let Some(json) = cmd.json_flag()
        {
            *json = true;
        }
        cmd
    }

    /// The name of the subcommand that the top-level `--json` was given to but that has no
    /// JSON output.
    pub fn json_unsupported_by(&self) -> Option<&'static str> {
        let mut cmd = self.cmd.clone()?;
        (self.json && cmd.json_flag().is_none()).then(|| cmd.name())
    }
}

/// All subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum SubCommand {
    /// Enqueue a job for execution.
    ///
    /// The command is executed through the system shell (`sh -c`), so it needs proper shell
    /// escaping. The simplest way to keep your quoting intact is to wrap the whole command in
    /// single quotes, for example:
    ///
    /// gpueue add 'ls $HOME && echo "Some string"'
    ///
    /// Use `--` to separate gpueue's options from options of the command itself.
    #[command(trailing_var_arg = true, after_help = DELAY_FORMAT_HELP)]
    Add {
        /// The command to be added.
        #[arg(required = true, num_args(1..), value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,

        /// Working directory of the job (defaults to the current directory).
        #[arg(short = 'w', long, value_hint = ValueHint::DirPath)]
        working_directory: Option<PathBuf>,

        /// Escape special shell characters (" ", "&", "!", etc.).
        /// Beware: this disables nearly all shell syntax ("&&", "&>", ...).
        #[arg(verbatim_doc_comment, short, long)]
        escape: bool,

        /// Start the job right away, ignoring the group's parallel limit (administrators only).
        /// For other users the job is queued first among their own jobs instead.
        #[arg(short = 'i', long, conflicts_with = "stashed")]
        immediate: bool,

        /// Follow the job's output right away (requires --immediate).
        #[arg(long, requires = "immediate", conflicts_with = "print_task_id")]
        follow: bool,

        /// Create the job in the Stashed state, so it won't be started automatically.
        #[arg(short, long, conflicts_with = "immediate")]
        stashed: bool,

        /// Don't enqueue the job before 'delay' has elapsed. See DELAY FORMAT below.
        #[arg(short, long, conflicts_with = "immediate", value_parser = parse_delay_until)]
        delay: Option<DateTime<Local>>,

        /// Put the job into this group (default: "default").
        ///
        /// Groups act as separate queues with their own parallel limit.
        /// Create groups with `gpueue group add`.
        #[arg(short, long)]
        group: Option<String>,

        /// Start the job only after all of these jobs finished successfully.
        ///
        /// If one of them fails, this job fails as well.
        /// Takes several ids, so end the list with `--` before the command.
        #[arg(short, long, num_args(1..))]
        after: Vec<usize>,

        /// Priority of the job among your own jobs; higher numbers are started first. May be
        /// negative.
        ///
        /// With the default multifactor priority this only orders your own jobs (see `gpueue
        /// prio`); with `priority_type = "basic"` it orders every job, as in pueue.
        #[arg(short = 'o', long, allow_negative_numbers = true)]
        priority: Option<i32>,

        /// (gpueue) Lower the job's priority by this much (Slurm's `--nice`).
        ///
        /// Only administrators may give negative values.
        #[arg(long, allow_negative_numbers = true)]
        nice: Option<i32>,

        /// A free-form label shown in the status table.
        #[arg(short, long)]
        label: Option<String>,

        /// Print only the job id, which is handy in scripts.
        #[arg(short, long)]
        print_task_id: bool,

        /// (gpueue) GPUs the job runs on, e.g. `0` or `0,1`.
        ///
        /// The job is only started once every listed GPU has a free slot.
        /// `CUDA_VISIBLE_DEVICES` is set accordingly. Without this flag the job is CPU-only.
        #[arg(long, value_name = "LIST", value_delimiter = ',', num_args = 1)]
        gpu: Vec<u32>,

        /// (gpueue) Never checkpoint this job; on shutdown it is terminated instead.
        #[arg(long)]
        no_checkpoint: bool,
    },

    /// Remove jobs from the list.
    ///
    /// Running or paused jobs have to be killed first. The checkpoint image of a Checkpointed
    /// job is deleted as well.
    #[command(alias("rm"))]
    Remove {
        /// The ids of the jobs to remove.
        #[arg(required = true, value_name = "IDS")]
        task_ids: Vec<usize>,
    },

    /// Swap the queue position of two jobs.
    ///
    /// Only works on queued and stashed jobs. With the default multifactor priority the
    /// position only matters among the owner's own jobs.
    Switch {
        /// The first job id.
        #[arg(value_name = "ID_1")]
        task_id_1: usize,
        /// The second job id.
        #[arg(value_name = "ID_2")]
        task_id_2: usize,
    },

    /// Stash jobs so they won't be started automatically.
    ///
    /// Use `gpueue enqueue` to put them back into the queue, or `gpueue start <id>` to start them.
    #[command(after_help = DELAY_FORMAT_HELP)]
    Stash {
        /// Stash these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Stash all queued jobs of a group.
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Stash all queued jobs in all groups.
        #[arg(short, long)]
        all: bool,

        /// Re-enqueue the stashed jobs once 'delay' has elapsed. See DELAY FORMAT below.
        #[arg(short, long, value_parser = parse_delay_until)]
        delay: Option<DateTime<Local>>,
    },

    /// Enqueue stashed jobs again.
    ///
    /// Without arguments, all stashed jobs of the default group are enqueued.
    #[command(after_help = DELAY_FORMAT_HELP)]
    Enqueue {
        /// Enqueue these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Enqueue all stashed jobs of a group.
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Enqueue all stashed jobs in all groups.
        #[arg(short, long)]
        all: bool,

        /// Enqueue only once 'delay' has elapsed. See DELAY FORMAT below.
        #[arg(short, long, value_parser = parse_delay_until)]
        delay: Option<DateTime<Local>>,
    },

    /// Resume paused jobs or groups, or force-start queued jobs.
    ///
    /// Without arguments, resumes the default group and its paused jobs. Only administrators
    /// resume groups; other users resume their own paused jobs in them (`-g`, `-a`).
    /// Starting specific queued/stashed jobs ignores group limits and dependencies.
    /// Checkpointed jobs are resumed with `gpueue restore`, not with `start`.
    Start {
        /// Start these jobs: paused jobs are resumed, queued/stashed jobs are force-started.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Resume a group and all paused jobs in it (other users: your paused jobs in it).
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Resume all groups and all paused jobs (other users: all your paused jobs).
        #[arg(short, long)]
        all: bool,
    },

    /// Restart finished jobs.
    ///
    /// By default an identical new job is created and enqueued. With `--in-place` the existing
    /// job is reused. Unlike pueue, Checkpointed jobs can be restarted too: they start over from
    /// the beginning as a new job and their checkpoint image is kept.
    #[command(alias("re"))]
    Restart {
        /// Restart these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Restart all failed jobs in all groups.
        #[arg(short, long)]
        all_failed: bool,

        /// Restart all failed jobs of a specific group.
        #[arg(short = 'g', long, conflicts_with = "all_failed")]
        failed_in_group: Option<String>,

        /// Start the new jobs right away, ignoring free slots and dependencies (administrators only).
        /// For other users they are queued first among their own jobs instead.
        #[arg(short = 'k', long, conflicts_with = "stashed")]
        immediate: bool,

        /// Put the restarted jobs into the Stashed state.
        #[arg(short, long)]
        stashed: bool,

        /// Reuse the existing jobs instead of creating new ones (overwrites their logs).
        #[arg(short, long)]
        in_place: bool,

        /// Create new identical jobs (the default).
        #[arg(long, conflicts_with = "in_place")]
        not_in_place: bool,

        /// Edit the jobs before restarting them (not implemented yet).
        #[arg(short, long)]
        edit: bool,
    },

    /// Pause running jobs or whole groups.
    ///
    /// Without arguments, pauses the default group and its jobs.
    /// A paused group does not start new jobs. Only administrators pause groups; other users
    /// pause their own jobs in them (`-g`, `-a`).
    Pause {
        /// Pause these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Pause a group and its jobs (other users: your jobs in it).
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Pause all groups and all jobs (other users: all your jobs).
        #[arg(short, long)]
        all: bool,

        /// Only pause the groups; let their running jobs finish (administrators).
        #[arg(short, long)]
        wait: bool,
    },

    /// Kill running jobs or all jobs of a group.
    ///
    /// Without ids, kills the jobs of the default group. Checkpointed jobs are not running;
    /// use `gpueue remove` to discard them. With `-g`/`-a`, administrators also pause the
    /// groups (as pueue does); other users kill their own jobs and leave the groups alone.
    Kill {
        /// Kill these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Kill all running jobs of a group and pause it (other users: your jobs in it).
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Kill all running jobs and pause all groups (other users: all your jobs).
        #[arg(short, long)]
        all: bool,

        /// Send this UNIX signal instead of killing the job.
        ///
        /// Accepts the name with or without `SIG` prefix in any case, or the number,
        /// e.g. `sigint`, `INT` and `2` are equivalent.
        #[arg(short, long, value_parser = parse_signal)]
        signal: Option<Signal>,
    },

    /// Not supported by gpueue: jobs have no stdin to send input to.
    ///
    /// gpueue runs every job with stdin connected to /dev/null. Make the command
    /// non-interactive instead (e.g. `yes | cmd` or a `--yes` flag).
    Send {
        /// The id of the job.
        #[arg(value_name = "ID")]
        task_id: usize,

        /// The input to send.
        input: String,
    },

    /// Edit properties of stashed or queued jobs in $EDITOR (not implemented yet).
    Edit {
        /// The ids of the jobs to edit.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,
    },

    /// Add or remove environment variables of a job (not implemented yet).
    Env {
        #[command(subcommand)]
        cmd: EnvCommand,
    },

    /// Add or remove groups; without a subcommand, list all groups.
    Group {
        /// Print the list of groups as JSON.
        #[arg(short, long)]
        json: bool,

        #[command(subcommand)]
        cmd: Option<GroupCommand>,
    },

    /// Show the status of all jobs.
    ///
    /// Jobs of other users are listed as well (with their owner), but their command details
    /// are hidden.
    Status {
        /// Filter, order or limit the listed jobs, e.g. `status=running first 5`.
        /// Use `--help` for the full syntax.
        #[arg(long_help = STATUS_QUERY_HELP)]
        query: Vec<String>,

        /// Print the state as JSON (gpueue's own documented schema).
        #[arg(short, long)]
        json: bool,

        /// Only show jobs of this group.
        #[arg(short, long)]
        group: Option<String>,
    },

    /// Show the output of jobs.
    ///
    /// Only the last few lines are shown by default.
    /// Use "follow" to stream the output of a running job.
    Log {
        /// Show the output of these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Show the output of this group's jobs.
        #[arg(short, long)]
        group: Option<String>,

        /// Show the output of the jobs in all groups.
        #[arg(short, long)]
        all: bool,

        /// Print jobs and their output as JSON.
        ///
        /// Only the last lines are included unless --full is given. The JSON is not streamed,
        /// so huge logs with --full may use a lot of memory.
        #[arg(short, long)]
        json: bool,

        /// Only print the last N lines of each job's output (default: 15, unless --full).
        #[arg(short, long, conflicts_with = "full")]
        lines: Option<usize>,

        /// Print the whole output.
        #[arg(short, long)]
        full: bool,
    },

    /// Follow the output of a running job, like "tail -f".
    #[command(alias("fo"))]
    Follow {
        /// The job to follow.
        ///
        /// May be omitted if exactly one job is running.
        #[arg(value_name = "ID")]
        task_id: Option<usize>,

        /// Only print the last N lines before following.
        #[arg(short, long)]
        lines: Option<usize>,
    },

    /// Wait until jobs are finished.
    ///
    /// Without arguments, waits for all jobs of the default group.
    /// Jobs in any non-finished state (queued, stashed, paused, ...) are waited for as well.
    Wait {
        /// Wait for these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Wait for all jobs of a group.
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Wait for all jobs in all groups.
        #[arg(short, long)]
        all: bool,

        /// Don't print status changes while waiting.
        #[arg(short, long)]
        quiet: bool,

        /// Wait until the jobs reach this status (default: done).
        #[arg(short, long, value_enum, ignore_case = true)]
        status: Option<WaitTargetStatus>,
    },

    /// Remove all finished jobs from the list.
    ///
    /// Checkpoint images of removed jobs are deleted as well.
    #[command(aliases(["cleanup", "clear"]))]
    Clean {
        /// Only remove jobs that finished successfully.
        #[arg(short, long)]
        successful_only: bool,

        /// Only remove jobs of this group.
        #[arg(short, long)]
        group: Option<String>,
    },

    /// Kill all jobs, remove them and reset the groups.
    ///
    /// Regular users only reset their own jobs; administrators reset everything.
    Reset {
        /// Only reset these groups (may be given multiple times).
        #[arg(short, long)]
        groups: Vec<String>,

        /// Don't ask for confirmation.
        #[arg(short, long)]
        force: bool,
    },

    /// Checkpoint all running jobs and stop the daemon (administrators only).
    ///
    /// Unlike pueue, running jobs are not killed but checkpointed, so they can be restored
    /// after the daemon is started again.
    Shutdown {
        /// Kill running jobs instead of checkpointing them.
        #[arg(long)]
        no_checkpoint: bool,
    },

    /// Set the number of jobs a group may run in parallel.
    ///
    /// Adjusts the default group unless --group is given. Lowering the limit does not stop
    /// running jobs; it only affects scheduling.
    Parallel {
        /// The number of parallel jobs; 0 means unlimited.
        #[arg(value_name = "PARALLEL_JOBS")]
        parallel_tasks: Option<usize>,

        /// Set the limit of this group.
        #[arg(short, long)]
        group: Option<String>,
    },

    /// Generate shell completion files.
    Completions {
        /// The target shell.
        #[arg(value_enum)]
        shell: Shell,

        /// Write the file into this directory instead of printing it to stdout.
        #[arg(value_hint = ValueHint::DirPath)]
        output_directory: Option<PathBuf>,
    },

    /// (gpueue) Checkpoint running jobs and stop them.
    ///
    /// The whole process state (CPU memory and GPU memory) is saved with CRIU and
    /// cuda-checkpoint. The job becomes Checkpointed and can be resumed with `gpueue restore`.
    Checkpoint {
        /// Checkpoint these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Checkpoint all running jobs of a group.
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Checkpoint all running jobs (your own; all users' jobs for administrators).
        #[arg(short, long)]
        all: bool,
    },

    /// (gpueue) Resume Checkpointed jobs.
    ///
    /// The jobs are queued again and resumed once their group and GPUs have free slots.
    /// Restoring fails (and keeps the image) if the GPU model, driver or GPU index differs from
    /// the time the checkpoint was taken.
    Restore {
        /// Restore these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Restore all Checkpointed jobs of a group.
        #[arg(short, long, conflicts_with = "all")]
        group: Option<String>,

        /// Restore all Checkpointed jobs (your own; all users' jobs for administrators).
        #[arg(short, long)]
        all: bool,

        /// Resume right away without waiting for free slots (administrators only).
        /// For other users the restore is queued first among their own jobs instead.
        #[arg(short, long)]
        immediate: bool,
    },

    /// (gpueue) Show the GPUs and their slots; see subcommands to change limits.
    Gpu {
        /// Print the list of GPUs as JSON.
        #[arg(short, long)]
        json: bool,

        #[command(subcommand)]
        cmd: Option<GpuCommand>,
    },

    /// (gpueue) Show the priority of pending jobs and its factors (like Slurm's `sprio`).
    ///
    /// Queued and restore-queued jobs are listed in the order they are considered for
    /// starting, with the weighted factors their priority is the sum of. Every user's jobs are
    /// listed, but never their commands.
    Prio {
        /// Only show these jobs.
        #[arg(value_name = "IDS")]
        task_ids: Vec<usize>,

        /// Print as JSON.
        #[arg(short, long)]
        json: bool,
    },

    /// (gpueue) Set the nice value of jobs (like Slurm's nice option): it is subtracted from
    /// their priority.
    ///
    /// Users may only raise the nice value of their own jobs (never below 0); administrators
    /// may set any value, including negative ones, on any job.
    #[command(allow_negative_numbers = true)]
    Nice {
        /// The ids of the jobs.
        #[arg(value_name = "IDS", required = true, num_args(1..))]
        task_ids: Vec<usize>,

        /// The new nice value.
        #[arg(value_name = "NICE", allow_negative_numbers = true)]
        nice: i32,
    },

    /// (gpueue) Move jobs to the top of their owner's queue (Slurm's `scontrol top`).
    ///
    /// The jobs go before the owner's other jobs, in the order given, but not before other
    /// users' jobs. Users may move their own jobs unless `enable_user_top = false`;
    /// administrators may move any job.
    Top {
        /// The ids of the jobs.
        #[arg(value_name = "IDS", required = true)]
        task_ids: Vec<usize>,
    },

    /// (gpueue) Set the site factor of jobs, which is added to their priority (administrators
    /// only).
    #[command(allow_negative_numbers = true)]
    Site {
        /// The ids of the jobs.
        #[arg(value_name = "IDS", required = true, num_args(1..))]
        task_ids: Vec<usize>,

        /// The new site factor.
        #[arg(value_name = "SITE", allow_negative_numbers = true)]
        site: i32,
    },

    /// (gpueue) Show the fair-share state of each user (like Slurm's `sshare`).
    ///
    /// Lists users with unfinished jobs or recorded usage, and users whose shares were set.
    Share {
        /// Print as JSON.
        #[arg(short, long)]
        json: bool,

        #[command(subcommand)]
        cmd: Option<ShareCommand>,
    },

    /// (gpueue) Run the daemon in the foreground (normally started by systemd).
    ///
    /// The global --socket option sets the socket to listen on.
    Daemon {
        /// Configuration file.
        #[arg(long, value_hint = ValueHint::FilePath, default_value = "/etc/gpueue/config.toml")]
        config: PathBuf,

        /// Directory for the state database and job logs [default: from the config file,
        /// /var/lib/gpueue].
        #[arg(long, value_hint = ValueHint::DirPath)]
        data_dir: Option<PathBuf>,

        /// Socket of the root helper [default: from the config file,
        /// /run/gpueue/helper.sock].
        #[arg(long, value_hint = ValueHint::FilePath)]
        helper_socket: Option<PathBuf>,
    },

    /// (gpueue) Install, uninstall or inspect the systemd services.
    Service {
        #[command(subcommand)]
        cmd: ServiceCommand,
    },

    /// (gpueue) Uninstall gpueue (requires root).
    ///
    /// Stops, disables and removes the systemd units, the binaries `gpueue service install`
    /// copied (as recorded in /var/lib/gpueue/install-receipt; binaries it used in place are
    /// kept) and its Nix GC root. Configuration, state, logs and checkpoint images are kept
    /// unless --purge is given. Only for an installation made by `gpueue service install` (or
    /// install.sh); packages are removed with their package manager.
    Uninstall {
        /// Also delete /etc/gpueue, /var/lib/gpueue (state, logs, checkpoint images) and the
        /// runtime directories.
        #[arg(long)]
        purge: bool,

        /// Do not ask for confirmation before purging.
        #[arg(short, long)]
        yes: bool,
    },

    /// (gpueue) Check the installation and print how to fix problems.
    ///
    /// Checks criu, cuda-checkpoint, the NVIDIA driver, the kernel, systemd, the daemon and
    /// helper services and sockets, the checkpoint storage and the configuration. Exits with a
    /// non-zero status if any check fails.
    Doctor,

    /// (gpueue) Update gpueue itself (root; without root it only checks).
    ///
    /// Downloads a release from GitHub, checks its signature and checksum, and installs it into
    /// the installed directory with its own `gpueue service install`. Running jobs keep
    /// running. Only for an installation made by `gpueue service install` (or install.sh);
    /// packages are updated with their package manager. Without root it only checks, like
    /// --check. A GitHub token is optional (it raises the API rate limit): set GH_TOKEN, or save
    /// one with `install.sh --save-token` (/etc/gpueue/github-token).
    Update {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,

        /// Install this version instead of the latest release (an older one only if it can
        /// still read the database).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,

        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
}

impl SubCommand {
    /// `status` without any arguments, which is what runs when no subcommand is given.
    pub fn default_status() -> Self {
        SubCommand::Status {
            query: Vec::new(),
            json: false,
            group: None,
        }
    }

    /// The `--json` flag of a subcommand that has one.
    fn json_flag(&mut self) -> Option<&mut bool> {
        match self {
            SubCommand::Status { json, .. }
            | SubCommand::Log { json, .. }
            | SubCommand::Group { json, .. }
            | SubCommand::Gpu { json, .. }
            | SubCommand::Prio { json, .. }
            | SubCommand::Share { json, .. } => Some(json),
            _ => None,
        }
    }

    /// The name of the subcommand as typed on the command line (canonical, no alias).
    pub fn name(&self) -> &'static str {
        match self {
            SubCommand::Add { .. } => "add",
            SubCommand::Remove { .. } => "remove",
            SubCommand::Switch { .. } => "switch",
            SubCommand::Stash { .. } => "stash",
            SubCommand::Enqueue { .. } => "enqueue",
            SubCommand::Start { .. } => "start",
            SubCommand::Restart { .. } => "restart",
            SubCommand::Pause { .. } => "pause",
            SubCommand::Kill { .. } => "kill",
            SubCommand::Send { .. } => "send",
            SubCommand::Edit { .. } => "edit",
            SubCommand::Env { .. } => "env",
            SubCommand::Group { .. } => "group",
            SubCommand::Status { .. } => "status",
            SubCommand::Log { .. } => "log",
            SubCommand::Follow { .. } => "follow",
            SubCommand::Wait { .. } => "wait",
            SubCommand::Clean { .. } => "clean",
            SubCommand::Reset { .. } => "reset",
            SubCommand::Shutdown { .. } => "shutdown",
            SubCommand::Parallel { .. } => "parallel",
            SubCommand::Completions { .. } => "completions",
            SubCommand::Checkpoint { .. } => "checkpoint",
            SubCommand::Restore { .. } => "restore",
            SubCommand::Gpu { .. } => "gpu",
            SubCommand::Prio { .. } => "prio",
            SubCommand::Nice { .. } => "nice",
            SubCommand::Top { .. } => "top",
            SubCommand::Site { .. } => "site",
            SubCommand::Share { .. } => "share",
            SubCommand::Daemon { .. } => "daemon",
            SubCommand::Service { .. } => "service",
            SubCommand::Uninstall { .. } => "uninstall",
            SubCommand::Doctor => "doctor",
            SubCommand::Update { .. } => "update",
        }
    }
}

/// Subcommands of `env`.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum EnvCommand {
    /// Set an environment variable of a job.
    Set {
        /// The id of the job.
        #[arg(value_name = "ID")]
        task_id: usize,
        /// The name of the variable.
        key: String,
        /// The value of the variable.
        value: String,
    },

    /// Remove an environment variable from a job.
    Unset {
        /// The id of the job.
        #[arg(value_name = "ID")]
        task_id: usize,
        /// The name of the variable.
        key: String,
    },
}

/// Subcommands of `group`.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum GroupCommand {
    /// Add a group (administrators only).
    Add {
        /// Name of the new group.
        name: String,

        /// Number of jobs the group may run in parallel; 0 means unlimited.
        #[arg(short, long)]
        parallel: Option<usize>,
    },

    /// Remove a group (administrators only); its jobs move to the default group.
    Remove {
        /// Name of the group to remove.
        name: String,
    },

    /// (gpueue) Set a group's priority (administrators only).
    ///
    /// A group's partition factor is its priority over the highest group priority; it counts
    /// with `priority_weight_partition` (Slurm's partition `PriorityJobFactor`).
    Priority {
        /// Name of the group.
        name: String,

        /// The new priority (0 or more).
        priority: u32,
    },
}

/// Subcommands of `share`.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum ShareCommand {
    /// Set a user's shares (administrators only; default 1).
    ///
    /// A user's normalised share is their shares over the shares of all active users.
    Set {
        /// User name or uid.
        user: String,

        /// The new number of shares (0 or more).
        shares: u32,
    },
}

/// Subcommands of `gpu`.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum GpuCommand {
    /// Set how many jobs may run on a GPU at the same time (administrators only).
    Parallel {
        /// Maximum number of concurrent jobs per GPU.
        parallel_jobs: usize,

        /// Only change these GPUs, e.g. `0` or `0,1` (default: all GPUs).
        #[arg(long, value_name = "LIST", value_delimiter = ',', num_args = 1)]
        gpu: Vec<u32>,
    },
}

/// Subcommands of `service`.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum ServiceCommand {
    /// Install the systemd units, the system user and group, the directories and a default
    /// configuration, then enable and start the services (requires root). Safe to re-run, e.g.
    /// after upgrading the binaries.
    ///
    /// Where the units run `gpueue` and `gpueue-helper` from (`gpueue-helper` must be next to
    /// `gpueue`): the directory of the current installation; else the directory this `gpueue`
    /// binary is in, when that is /usr/bin or /usr/local/bin; else copies of both binaries in
    /// /usr/local/bin. What was copied is recorded in /var/lib/gpueue/install-receipt for
    /// `gpueue uninstall`.
    Install {
        /// Enable the services but do not start (or restart) them now.
        #[arg(long)]
        no_start: bool,

        /// Install somewhere else than the default: copy `gpueue` and `gpueue-helper` into this
        /// directory and run them from there. It and its parents must be owned by root and not
        /// group/world-writable (gpueue-helper runs as root).
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        bin_dir: Option<PathBuf>,

        /// Overwrite unit, sysusers.d and tmpfiles.d files in /etc that were not written by
        /// `gpueue service install` (by default they are left alone and install stops).
        #[arg(long)]
        force: bool,
    },

    /// Same as `gpueue uninstall`: stop, disable and remove the systemd units and the binaries
    /// `gpueue service install` copied (requires root). Configuration, state, logs and
    /// checkpoint images are kept unless --purge is given.
    Uninstall {
        /// Also delete /etc/gpueue, /var/lib/gpueue (state, logs, checkpoint images) and the
        /// runtime directories.
        #[arg(long)]
        purge: bool,

        /// Do not ask for confirmation before purging.
        #[arg(short, long)]
        yes: bool,
    },

    /// Show the state of both services and whether daemon and helper are connected.
    Status,

    /// Print how gpueue is installed as one word: none, service-install, package, unowned, nix,
    /// manual or mixed (used by install.sh; before v2 it printed nixos instead of nix).
    #[command(hide = true)]
    InstallMethod,

    /// Print this binary's version and database schema version as JSON (used by
    /// `gpueue update`).
    #[command(hide = true)]
    VersionInfo,
}

/// When to colorize output.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Never,
    Always,
}

/// Shells supported by `completions`.
///
/// `PowerShell` keeps pueue's spelling so the accepted value stays `power-shell`.
#[allow(clippy::enum_variant_names)]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    Nushell,
}

/// Status `wait` waits for.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitTargetStatus {
    /// The jobs finished (successfully or not).
    #[default]
    Done,
    /// The jobs finished successfully.
    Success,
    /// The jobs are queued.
    Queued,
    /// The jobs are running.
    Running,
}

/// A UNIX signal that `kill --signal` may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    SigHup,
    SigInt,
    SigQuit,
    SigKill,
    SigUsr1,
    SigUsr2,
    SigTerm,
    SigCont,
    SigStop,
    SigTstp,
}

impl Signal {
    const ALL: [Signal; 10] = [
        Signal::SigHup,
        Signal::SigInt,
        Signal::SigQuit,
        Signal::SigKill,
        Signal::SigUsr1,
        Signal::SigUsr2,
        Signal::SigTerm,
        Signal::SigCont,
        Signal::SigStop,
        Signal::SigTstp,
    ];

    /// The signal number on Linux.
    pub fn number(self) -> i32 {
        match self {
            Signal::SigHup => 1,
            Signal::SigInt => 2,
            Signal::SigQuit => 3,
            Signal::SigKill => 9,
            Signal::SigUsr1 => 10,
            Signal::SigUsr2 => 12,
            Signal::SigTerm => 15,
            Signal::SigCont => 18,
            Signal::SigStop => 19,
            Signal::SigTstp => 20,
        }
    }

    /// The short upper-case name without the `SIG` prefix.
    pub fn short_name(self) -> &'static str {
        match self {
            Signal::SigHup => "HUP",
            Signal::SigInt => "INT",
            Signal::SigQuit => "QUIT",
            Signal::SigKill => "KILL",
            Signal::SigUsr1 => "USR1",
            Signal::SigUsr2 => "USR2",
            Signal::SigTerm => "TERM",
            Signal::SigCont => "CONT",
            Signal::SigStop => "STOP",
            Signal::SigTstp => "TSTP",
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SIG{}", self.short_name())
    }
}

impl FromStr for Signal {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if let Ok(number) = trimmed.parse::<i32>() {
            return Signal::ALL
                .into_iter()
                .find(|signal| signal.number() == number)
                .ok_or_else(|| format!("unsupported signal number {number}"));
        }
        let upper = trimmed.to_ascii_uppercase();
        let name = upper.strip_prefix("SIG").unwrap_or(&upper);
        Signal::ALL
            .into_iter()
            .find(|signal| signal.short_name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Signal::ALL.iter().map(|s| s.short_name()).collect();
                format!(
                    "unknown signal '{s}' (expected one of {} or its number)",
                    names.join(", ")
                )
            })
    }
}

fn parse_signal(src: &str) -> Result<Signal, String> {
    src.parse()
}

/// Parse a `--delay` value: a number of seconds or a date expression (same rules as pueue).
pub fn parse_delay_until(src: &str) -> Result<DateTime<Local>, String> {
    parse_delay_until_from(src, Local::now())
}

fn parse_delay_until_from(src: &str, now: DateTime<Local>) -> Result<DateTime<Local>, String> {
    if let Ok(seconds) = src.parse::<i64>() {
        let delta = TimeDelta::try_seconds(seconds)
            .ok_or_else(|| format!("{seconds} seconds is out of range"))?;
        return now
            .checked_add_signed(delta)
            .ok_or_else(|| format!("{seconds} seconds is out of range"));
    }

    interim::parse_date_string(src, now, interim::Dialect::Us)
        .map_err(|_| String::from("could not parse as seconds or date expression"))
}

#[cfg(test)]
mod tests;
