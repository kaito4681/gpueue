//! Rendering daemon data for humans, modelled on pueue's output.
//!
//! Everything here is pure (data in, `String` out) so it can be tested
//! without a terminal or a daemon.

use std::borrow::Cow;
use std::collections::BTreeMap;

use comfy_table::presets::{NOTHING, UTF8_HORIZONTAL_ONLY};
use comfy_table::{Cell, CellAlignment, ContentArrangement, Row, Table};
use gpueue_core::authz::REDACTED;
use gpueue_core::model::{DEFAULT_GROUP, Group, JobId, JobResult, JobState};
use gpueue_core::priority::PriorityType;
use gpueue_protocol::api::{
    ActionOutcome, GpuView, JobActionResult, JobView, OrphanJob, PriorityResponse, QueuedLaunch,
    SharesResponse,
};

use crate::clock::{Clock, human_bytes};
use crate::query::Column;
use crate::style::{Color, Style};
use crate::users::UserNames;

/// Everything rendering needs besides the data.
pub struct RenderCtx {
    pub style: Style,
    pub clock: Clock,
    pub users: UserNames,
    /// Let comfy-table detect the terminal width (off in tests, so tables
    /// never wrap there).
    pub detect_width: bool,
}

impl RenderCtx {
    fn table(&self) -> Table {
        let mut table = Table::new();
        if !self.detect_width {
            table.force_no_tty();
        }
        table.set_content_arrangement(ContentArrangement::Dynamic);
        if self.style.enabled {
            table.enforce_styling();
        }
        table
    }
}

/// Makes text that came from the daemon (or from a job: labels, commands,
/// directories, error reasons) safe to print on a terminal: C0 and C1 control
/// characters, including ESC, are shown as `\xNN` / `\u{..}` escapes, and a
/// newline as the visible marker `⏎`. Only job logs (`log`, `follow`) are
/// written raw, as pueue does.
pub fn escape_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' => out.push('⏎'),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() && (c as u32) < 0x100 => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Job states
// ---------------------------------------------------------------------------

/// The state as shown in the status table, with its color (pueue's names
/// and colors, plus the gpueue checkpoint states).
pub fn state_text(job: &JobView) -> (String, Color) {
    match &job.state {
        JobState::Queued { .. } => ("Queued".into(), Color::Yellow),
        JobState::Stashed { .. } => ("Stashed".into(), Color::Yellow),
        JobState::Running => ("Running".into(), Color::Green),
        JobState::Paused => ("Paused".into(), Color::White),
        JobState::Checkpointing => ("Checkpointing".into(), Color::Yellow),
        JobState::Checkpointed => {
            // The last error of a checkpointed job is a failed restore (only
            // in the owner's view; `gpueue log` shows the reason).
            let mut notes = Vec::new();
            if let Some(size) = job.checkpoint_size_bytes {
                notes.push(human_bytes(size));
            }
            let restore_failed = last_error(job).is_some();
            if restore_failed {
                notes.push("restore failed".into());
            }
            let text = if notes.is_empty() {
                "Checkpointed".into()
            } else {
                format!("Checkpointed ({})", notes.join(", "))
            };
            (
                text,
                if restore_failed {
                    Color::Red
                } else {
                    Color::Cyan
                },
            )
        }
        JobState::RestoreQueued { .. } => ("Restore queued".into(), Color::Yellow),
        JobState::Restoring => ("Restoring".into(), Color::Yellow),
        JobState::Terminating { .. } => ("Terminating".into(), Color::Red),
        JobState::Done { result } => match result {
            JobResult::Success => ("Success".into(), Color::Green),
            JobResult::Failed { exit_code } => (format!("Failed ({exit_code})"), Color::Red),
            JobResult::FailedToSpawn { .. } => ("Failed to spawn".into(), Color::Red),
            JobResult::Killed => ("Killed".into(), Color::Red),
            JobResult::DependencyFailed => ("Dependency failed".into(), Color::Red),
            JobResult::CheckpointFailed { .. } => ("Checkpoint failed".into(), Color::Red),
            JobResult::Lost => ("Lost".into(), Color::Red),
        },
    }
}

/// The last error the daemon recorded for the job (owner's view only).
fn last_error(job: &JobView) -> Option<&str> {
    job.details.as_ref()?.last_error.as_deref()
}

/// Short state name for `wait` messages (`Queued`, `Running`, ...).
pub fn state_name(state: &JobState) -> &'static str {
    match state {
        JobState::Queued { .. } => "Queued",
        JobState::Stashed { .. } => "Stashed",
        JobState::Running => "Running",
        JobState::Paused => "Paused",
        JobState::Checkpointing => "Checkpointing",
        JobState::Checkpointed => "Checkpointed",
        JobState::RestoreQueued { .. } => "Restore queued",
        JobState::Restoring => "Restoring",
        JobState::Terminating { .. } => "Terminating",
        JobState::Done { result } => match result {
            JobResult::Success => "Success",
            _ => "Failed",
        },
    }
}

fn state_color(state: &JobState) -> Color {
    match state {
        JobState::Running => Color::Green,
        JobState::Done { result } if *result == JobResult::Success => Color::Green,
        JobState::Done { .. } | JobState::Terminating { .. } => Color::Red,
        JobState::Checkpointed => Color::Cyan,
        _ => Color::White,
    }
}

/// Whether the job has (had) a process and therefore a log.
pub fn has_log(state: &JobState) -> bool {
    !matches!(state, JobState::Queued { .. } | JobState::Stashed { .. })
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// `Group "name" (N parallel): running`, as pueue's group headline.
pub fn group_headline(name: &str, group: Option<&Group>, style: &Style) -> String {
    let title = style.bold(format!("Group \"{}\"", escape_controls(name)));
    let Some(group) = group else {
        return title;
    };
    let status = if group.paused {
        style.paint("paused", Some(Color::Yellow), false)
    } else {
        style.paint("running", Some(Color::Green), false)
    };
    format!("{title} ({} parallel): {status}", group.parallel)
}

/// The output of `gpueue group` (one headline per group).
pub fn groups(groups: &[Group], style: &Style) -> String {
    groups
        .iter()
        .map(|group| group_headline(&group.name, Some(group), style))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// The columns of the status table, decided like pueue's `TableBuilder`.
#[derive(Debug, Clone)]
struct Columns(Vec<Column>);

impl Columns {
    /// Explicit `columns=` selection, or the defaults plus the optional
    /// columns some job needs.
    fn for_jobs(selected: &[Column], jobs: &[JobView]) -> Self {
        if !selected.is_empty() {
            return Self(selected.to_vec());
        }
        let mut columns = vec![Column::Id, Column::Status];
        if jobs.iter().any(|job| job.priority != 0) {
            columns.push(Column::Priority);
        }
        if jobs.iter().any(|job| {
            matches!(
                job.state,
                JobState::Stashed {
                    enqueue_at: Some(_)
                }
            )
        }) {
            columns.push(Column::EnqueueAt);
        }
        if jobs.iter().any(|job| !job.dependencies.is_empty()) {
            columns.push(Column::Dependencies);
        }
        if jobs.iter().any(|job| job.label.is_some()) {
            columns.push(Column::Label);
        }
        columns.push(Column::Owner);
        if jobs.iter().any(|job| !job.gpus.is_empty()) {
            columns.push(Column::Gpu);
        }
        columns.extend([Column::Command, Column::Path, Column::Start, Column::End]);
        Self(columns)
    }

    fn header(&self) -> Vec<&'static str> {
        self.0
            .iter()
            .map(|column| match column {
                Column::Id => "Id",
                Column::Status => "Status",
                Column::Priority => "Prio",
                Column::EnqueueAt => "Enqueue At",
                Column::Dependencies => "Deps",
                Column::Label => "Label",
                Column::Owner => "Owner",
                Column::Gpu => "GPU",
                Column::Command => "Command",
                Column::Path => "Path",
                Column::Start => "Start",
                Column::End => "End",
            })
            .collect()
    }
}

fn join_ids<T: ToString>(ids: &[T]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn job_row(job: &JobView, columns: &Columns, ctx: &RenderCtx) -> Row {
    let mut row = Row::new();
    for column in &columns.0 {
        let cell = match column {
            Column::Id => Cell::new(job.id),
            Column::Status => {
                let (text, color) = state_text(job);
                ctx.style.cell(text, Some(color), false)
            }
            Column::Priority => Cell::new(job.priority),
            Column::EnqueueAt => match job.state {
                JobState::Stashed {
                    enqueue_at: Some(at),
                } => Cell::new(ctx.clock.format_short(at)),
                _ => Cell::new(""),
            },
            Column::Dependencies => Cell::new(join_ids(&job.dependencies)),
            Column::Label => Cell::new(escape_controls(job.label.as_deref().unwrap_or_default())),
            Column::Owner => Cell::new(ctx.users.name(job.owner)),
            Column::Gpu => Cell::new(
                job.gpus
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Column::Command => match &job.details {
                Some(details) => Cell::new(escape_controls(&details.command)),
                None => Cell::new(REDACTED),
            },
            Column::Path => match &job.details {
                Some(details) => Cell::new(escape_controls(&details.cwd.to_string_lossy())),
                None => Cell::new(REDACTED),
            },
            Column::Start => Cell::new(
                job.started_at
                    .map(|at| ctx.clock.format_short(at))
                    .unwrap_or_default(),
            ),
            Column::End => Cell::new(
                job.finished_at
                    .map(|at| ctx.clock.format_short(at))
                    .unwrap_or_default(),
            ),
        };
        row.add_cell(cell);
    }
    row
}

fn job_table(jobs: &[JobView], selected: &[Column], ctx: &RenderCtx) -> String {
    let columns = Columns::for_jobs(selected, jobs);
    let mut table = ctx.table();
    table
        .load_preset(UTF8_HORIZONTAL_ONLY)
        .set_header(columns.header())
        .add_rows(jobs.iter().map(|job| job_row(job, &columns, ctx)));
    table.to_string()
}

/// Input of [`status`].
pub struct StatusInput<'a> {
    pub groups: &'a [Group],
    /// Jobs after applying the query.
    pub jobs: Vec<JobView>,
    /// Whether the daemon has no jobs at all (before filtering).
    pub no_jobs_at_all: bool,
    /// `--group`.
    pub group: Option<&'a str>,
    /// `columns=` of the query.
    pub columns: &'a [Column],
}

/// The status output: a headline and table per group, like pueue.
pub fn status(input: StatusInput<'_>, ctx: &RenderCtx) -> Result<String, String> {
    let find_group = |name: &str| input.groups.iter().find(|group| group.name == name);
    let mut by_group: BTreeMap<String, Vec<JobView>> = BTreeMap::new();
    for job in input.jobs {
        by_group.entry(job.group.clone()).or_default().push(job);
    }

    if let Some(name) = input.group {
        let group = find_group(name).ok_or_else(|| format!("There exists no group \"{name}\""))?;
        let mut out = group_headline(name, Some(group), &ctx.style);
        match by_group.get(name) {
            Some(jobs) if !jobs.is_empty() => {
                out.push('\n');
                out.push_str(&job_table(jobs, input.columns, ctx));
            }
            _ => out.push_str(&format!(
                "\nJob list is empty. Add jobs with `gpueue add -g {name} -- [cmd]`"
            )),
        }
        return Ok(out);
    }

    if input.no_jobs_at_all {
        let headline = group_headline(DEFAULT_GROUP, find_group(DEFAULT_GROUP), &ctx.style);
        return Ok(format!(
            "{headline}\n\nJob list is empty. Add jobs with `gpueue add -- [cmd]`"
        ));
    }

    // The default group first, then the others alphabetically.
    let mut sections = Vec::new();
    let default = by_group.remove(DEFAULT_GROUP);
    for (name, jobs) in default
        .map(|jobs| (DEFAULT_GROUP.to_owned(), jobs))
        .into_iter()
        .chain(by_group)
    {
        let headline = group_headline(&name, find_group(&name), &ctx.style);
        sections.push(format!(
            "{headline}\n{}",
            job_table(&jobs, input.columns, ctx)
        ));
    }
    Ok(sections.join("\n\n"))
}

// ---------------------------------------------------------------------------
// GPUs
// ---------------------------------------------------------------------------

/// The output of `gpueue gpu`.
pub fn gpus(gpus: &[GpuView], ctx: &RenderCtx) -> String {
    if gpus.is_empty() {
        return "No GPUs detected.".into();
    }
    let mut table = ctx.table();
    table
        .load_preset(UTF8_HORIZONTAL_ONLY)
        .set_header(vec!["GPU", "Name", "Memory", "Slots", "Jobs"]);
    for gpu in gpus {
        let limit = if gpu.parallel == 0 {
            "unlimited".to_owned()
        } else {
            gpu.parallel.to_string()
        };
        let slots_color = if gpu.parallel != 0 && gpu.used >= gpu.parallel {
            Color::Yellow
        } else {
            Color::Green
        };
        let jobs = gpu
            .jobs
            .iter()
            .map(|job| format!("{} ({})", job.id, ctx.users.name(job.owner)))
            .collect::<Vec<_>>()
            .join(", ");
        table.add_row(vec![
            Cell::new(gpu.index),
            if gpu.available {
                Cell::new(escape_controls(&gpu.name))
            } else {
                ctx.style.cell(
                    format!("{} (not detected)", escape_controls(&gpu.name)),
                    Some(Color::Red),
                    false,
                )
            },
            Cell::new(
                gpu.memory_total_bytes
                    .map(human_bytes)
                    .unwrap_or_else(|| "-".into()),
            ),
            ctx.style
                .cell(format!("{}/{limit}", gpu.used), Some(slots_color), false),
            Cell::new(jobs),
        ]);
    }
    table.to_string()
}

// ---------------------------------------------------------------------------
// Priority and fair share
// ---------------------------------------------------------------------------

/// A duration in seconds as `7d`, `5m`, `90s`, ... (the largest unit that
/// divides it).
pub fn human_secs(secs: u64) -> String {
    if secs == 0 {
        return "0s".into();
    }
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// Rounds a priority or weighted factor for display (Slurm shows integers).
fn whole(value: f64) -> String {
    let rounded = value.round();
    // Avoid printing "-0".
    if rounded == 0.0 {
        "0".into()
    } else {
        format!("{rounded:.0}")
    }
}

/// The output of `gpueue prio` (like Slurm's `sprio`): one row per pending
/// job, in scheduling order, with the weighted factors.
pub fn priorities(response: &PriorityResponse, ctx: &RenderCtx) -> String {
    let basic = response.priority_type == PriorityType::Basic;
    let headline = if basic {
        "priority_type = \"basic\": pending jobs are ordered by -o priority, then by id.".to_owned()
    } else {
        let w = &response.weights;
        let mut weights = format!(
            "Weights: age {}, fairshare {}, jobsize {}, partition {}",
            w.age, w.fairshare, w.job_size, w.partition
        );
        if !w.tres.is_empty() {
            weights.push_str(&format!(", tres {}", w.tres));
        }
        weights
    };
    if response.jobs.is_empty() {
        return format!("{headline}\nNo pending jobs.");
    }
    let mut table = ctx.table();
    table.load_preset(UTF8_HORIZONTAL_ONLY);
    if basic {
        table.set_header(vec!["Id", "Owner", "Group", "Prio"]);
    } else {
        table.set_header(vec![
            "Id",
            "Owner",
            "Group",
            "Priority",
            "Site",
            "Age",
            "Fairshare",
            "JobSize",
            "Partition",
            "TRES",
            "Nice",
        ]);
    }
    for job in &response.jobs {
        let mut row = vec![
            Cell::new(job.id),
            Cell::new(ctx.users.name(job.owner)),
            Cell::new(escape_controls(&job.group)),
            Cell::new(whole(job.priority)),
        ];
        if !basic {
            row.extend([
                Cell::new(job.site),
                Cell::new(whole(job.weighted.age)),
                Cell::new(whole(job.weighted.fairshare)),
                Cell::new(whole(job.weighted.job_size)),
                Cell::new(whole(job.weighted.partition)),
                Cell::new(whole(job.weighted.tres)),
                Cell::new(job.nice),
            ]);
        }
        table.add_row(
            row.into_iter()
                .map(|cell| cell.set_alignment(CellAlignment::Right)),
        );
    }
    format!("{headline}\n{table}")
}

/// The output of `gpueue share` (like Slurm's `sshare`).
pub fn shares(response: &SharesResponse, ctx: &RenderCtx) -> String {
    let decay = if response.decay_half_life_secs == 0 {
        "no decay".to_owned()
    } else {
        format!("half-life {}", human_secs(response.decay_half_life_secs))
    };
    let footer = format!(
        "Usage is billing × seconds (billing {}), {decay}, recalculated every {}, reset: {}.",
        if response.billing_weights.is_empty() {
            "0".to_owned()
        } else {
            response.billing_weights.clone()
        },
        human_secs(response.calc_period_secs),
        response.usage_reset_period,
    );
    if response.users.is_empty() {
        return format!("No users with jobs or usage.\n{footer}");
    }
    let mut table = ctx.table();
    table.load_preset(UTF8_HORIZONTAL_ONLY).set_header(vec![
        "User",
        "RawShares",
        "NormShares",
        "RawUsage",
        "EffectvUsage",
        "FairShare",
    ]);
    for user in &response.users {
        let mut name = ctx.users.name(user.uid);
        if !user.active {
            name.push_str(" (inactive)");
        }
        table.add_row(vec![
            Cell::new(name),
            Cell::new(user.shares).set_alignment(CellAlignment::Right),
            Cell::new(format!("{:.6}", user.norm_shares)).set_alignment(CellAlignment::Right),
            Cell::new(format!("{:.0}", user.raw_usage)).set_alignment(CellAlignment::Right),
            Cell::new(format!("{:.6}", user.effective_usage)).set_alignment(CellAlignment::Right),
            Cell::new(format!("{:.6}", user.fairshare)).set_alignment(CellAlignment::Right),
        ]);
    }
    format!("{table}\n{footer}")
}

// ---------------------------------------------------------------------------
// Action results
// ---------------------------------------------------------------------------

/// What to print after a (bulk) job action.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    /// Whether every job succeeded.
    pub ok: bool,
}

impl Report {
    pub fn print(&self) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        for line in &self.stdout {
            writeln!(stdout, "{line}")?;
        }
        for line in &self.stderr {
            eprintln!("{line}");
        }
        Ok(())
    }
}

/// Said when a job could not be started (or restored) right away because
/// the helper is not connected; the daemon launches it once it is.
pub fn launch_pending_note(id: JobId) -> String {
    format!(
        "Job {id} will start as soon as the gpueue helper is connected again \
         (it is not right now; see `gpueue doctor`)."
    )
}

/// Said when a non-administrator asked to start (or restore) a job right
/// away: only administrators do that, so the daemon queued it instead.
pub fn queued_note(id: JobId, queued: QueuedLaunch) -> String {
    let place = match queued {
        QueuedLaunch::First => "queued first among your jobs",
        QueuedLaunch::InLine => "queued",
    };
    format!(
        "Job {id} was {place} instead of being launched right away: only administrators \
         launch jobs immediately. It runs when its priority allows (see `gpueue prio`)."
    )
}

/// Summarizes per-job results like pueue (`{message}: 1, 2`) and lists every
/// failure with its reason.
///
/// `message` is printed with the ids that succeeded; `headline`, if given,
/// replaces it (used for group-wide actions, where pueue prints a fixed
/// sentence).
pub fn action_report(
    results: &[JobActionResult],
    message: &str,
    headline: Option<String>,
    style: &Style,
) -> Report {
    let mut report = Report {
        ok: true,
        ..Report::default()
    };
    let succeeded: Vec<&JobActionResult> = results.iter().filter(|r| r.is_ok()).collect();

    // A job a non-administrator asked to start or restore right away was
    // only queued: it is not listed as started, its note says what
    // happened. (`restart` itself did happen; its copy gets the note.)
    let launched_or_done = |result: &&&JobActionResult| {
        !matches!(
            &result.outcome,
            ActionOutcome::Ok {
                new_id: None,
                queued: Some(_),
                ..
            }
        )
    };
    if let Some(headline) = headline {
        report.stdout.push(headline);
    } else if succeeded.iter().any(|r| launched_or_done(&r)) {
        let ids: Vec<String> = succeeded
            .iter()
            .filter(launched_or_done)
            .map(|result| match &result.outcome {
                ActionOutcome::Ok {
                    new_id: Some(new_id),
                    ..
                } => format!("{} (new id {new_id})", result.id),
                _ => result.id.to_string(),
            })
            .collect();
        report.stdout.push(format!("{message}: {}", ids.join(", ")));
    }
    for result in &succeeded {
        if let ActionOutcome::Ok {
            new_id,
            launch_pending,
            queued,
        } = &result.outcome
        {
            let id = new_id.unwrap_or(result.id);
            if *launch_pending {
                report.stdout.push(launch_pending_note(id));
            } else if let Some(queued) = queued {
                report.stdout.push(queued_note(id, *queued));
            }
        }
    }

    for result in results {
        if let ActionOutcome::Error { error } = &result.outcome {
            report.ok = false;
            report.stderr.push(style.paint(
                format!("Job {}: {}", result.id, escape_controls(&error.message)),
                Some(Color::Red),
                false,
            ));
        }
    }
    if results.is_empty() && report.stdout.is_empty() {
        report.stdout.push("No matching jobs.".into());
    }
    report
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// The text after `Job N: ` in a log header.
fn log_state_text(job: &JobView) -> (String, Color) {
    match &job.state {
        JobState::Paused => ("paused".into(), Color::White),
        JobState::Running => ("running".into(), Color::Yellow),
        JobState::Checkpointing => ("checkpointing".into(), Color::Yellow),
        JobState::Checkpointed => ("checkpointed".into(), Color::Cyan),
        JobState::RestoreQueued { .. } => ("queued for restore".into(), Color::Yellow),
        JobState::Restoring => ("restoring".into(), Color::Yellow),
        JobState::Terminating { .. } => ("terminating".into(), Color::Red),
        JobState::Queued { .. } => ("queued".into(), Color::White),
        JobState::Stashed { .. } => ("stashed".into(), Color::White),
        JobState::Done { result } => match result {
            JobResult::Success => ("completed successfully".into(), Color::Green),
            JobResult::Failed { exit_code } => {
                (format!("failed with exit code {exit_code}"), Color::Red)
            }
            JobResult::FailedToSpawn { reason } => (
                format!("failed to spawn: {}", escape_controls(reason)),
                Color::Red,
            ),
            JobResult::Killed => ("killed by system or user".into(), Color::Red),
            JobResult::DependencyFailed => ("dependency failed".into(), Color::Red),
            JobResult::CheckpointFailed { reason } => (
                format!(
                    "terminated after a failed checkpoint: {}",
                    escape_controls(reason)
                ),
                Color::Red,
            ),
            JobResult::Lost => (
                "lost (it disappeared while the daemon was not running)".into(),
                Color::Red,
            ),
        },
    }
}

/// The box with the job id and state plus the details table shown above a
/// job's log (pueue's `print_task_info`).
pub fn log_header(job: &JobView, ctx: &RenderCtx) -> String {
    let (state, color) = log_state_text(job);
    let mut title = ctx.table();
    title.load_preset("││─ └──┘     ─ ┌┐  ");
    title.set_header(vec![
        ctx.style.cell(format!("Job {}: ", job.id), None, true),
        ctx.style.cell(&state, Some(color), false),
    ]);

    let mut info = ctx.table();
    info.load_preset(NOTHING);
    let mut add = |key: &str, value: String| {
        info.add_row(vec![ctx.style.cell(key, None, true), Cell::new(value)]);
    };
    if let Some(details) = &job.details {
        add("Command:", escape_controls(&details.command).into_owned());
        add(
            "Path:",
            escape_controls(&details.cwd.to_string_lossy()).into_owned(),
        );
    }
    if let Some(label) = &job.label {
        add("Label:", escape_controls(label).into_owned());
    }
    add("Owner:", ctx.users.name(job.owner));
    if !job.gpus.is_empty() {
        add("GPU:", join_ids(&job.gpus));
    }
    if let Some(start) = job.started_at {
        add("Start:", ctx.clock.format_long(start));
    }
    if let Some(end) = job.finished_at {
        add("End:", ctx.clock.format_long(end));
    }
    // Unless the title already says it (a failed spawn or dump).
    if let Some(error) = last_error(job)
        && !state.contains(&*escape_controls(error))
    {
        let label = match job.state {
            JobState::Checkpointed | JobState::RestoreQueued { .. } | JobState::Restoring => {
                "Restore failed:"
            }
            _ => "Last error:",
        };
        add(label, escape_controls(error).into_owned());
    }
    if let Some(column) = info.column_mut(0) {
        column.set_cell_alignment(CellAlignment::Right);
        column.set_padding((0, 0));
    }
    format!("{title}\n{info}")
}

/// `output:` line shown between the header and the log bytes.
pub fn log_output_line(truncated_to: Option<usize>, style: &Style) -> String {
    let header = style.paint("output:", Some(Color::Green), true);
    match truncated_to {
        Some(lines) => format!("{header} (last {lines} lines)"),
        None => header,
    }
}

/// The note `status` prints (to stderr) for administrators about jobs the
/// helper runs that the daemon does not account for.
pub fn orphans_note(orphans: &[OrphanJob]) -> String {
    let mut out = String::from(
        "Warning: the helper runs jobs the daemon does not account for (their GPUs stay \
         taken until they end; stop one with `gpueue kill <id>`):",
    );
    for orphan in orphans {
        out.push_str(&format!("\n  job {}", orphan.id));
        if let Some(owner) = orphan.owner {
            out.push_str(&format!(", uid {owner}"));
        }
        if let Some(pid) = orphan.pid {
            out.push_str(&format!(", pid {pid}"));
        }
        if !orphan.gpus.is_empty() {
            let gpus: Vec<String> = orphan.gpus.iter().map(ToString::to_string).collect();
            out.push_str(&format!(", GPUs {}", gpus.join(",")));
        }
    }
    out
}

/// Keeps the last `lines` lines of `log`. Returns the kept bytes and whether
/// anything was cut off. A trailing newline does not start another line.
pub fn last_lines(log: &[u8], lines: usize) -> (&[u8], bool) {
    if lines == 0 {
        return (&log[log.len()..], !log.is_empty());
    }
    let body = log.strip_suffix(b"\n").unwrap_or(log);
    let mut seen = 0;
    for (index, byte) in body.iter().enumerate().rev() {
        if *byte == b'\n' {
            seen += 1;
            if seen == lines {
                return (&log[index + 1..], true);
            }
        }
    }
    (log, false)
}

// ---------------------------------------------------------------------------
// wait
// ---------------------------------------------------------------------------

/// The time of a `wait` line: when it is printed, not when `wait` started.
fn wait_prefix(ctx: &RenderCtx) -> String {
    ctx.clock.current().format("%H:%M:%S").to_string()
}

/// `wait`: a job that appeared while waiting, or an active job found at the
/// start (`first_run`, only reported if it is running, as in pueue).
pub fn wait_new_job(job: &JobView, first_run: bool, ctx: &RenderCtx) -> Option<String> {
    let id = ctx.style.bold(job.id);
    let state = ctx
        .style
        .paint(state_name(&job.state), Some(state_color(&job.state)), false);
    let now = wait_prefix(ctx);
    if !first_run {
        return Some(format!("{now} - New job {id} with status {state}"));
    }
    matches!(job.state, JobState::Running)
        .then(|| format!("{now} - Found active job {id} with status {state}"))
}

/// `wait`: a job changed its state.
pub fn wait_state_change(id: JobId, previous: &JobState, job: &JobView, ctx: &RenderCtx) -> String {
    let style = &ctx.style;
    let now = wait_prefix(ctx);
    let id = style.bold(id);
    if let JobState::Done { result } = &job.state {
        let text = match result {
            JobResult::Success => format!(
                "Job {id} succeeded with {}",
                style.paint("0", Some(Color::Green), false)
            ),
            JobResult::DependencyFailed => format!(
                "Job {id} failed due to {}",
                style.paint("failed dependencies", Some(Color::Red), false)
            ),
            JobResult::FailedToSpawn { .. } => format!(
                "Job {id} {}",
                style.paint("failed to spawn", Some(Color::Red), false)
            ),
            JobResult::Failed { exit_code } => format!(
                "Job {id} failed with {}",
                style.paint(exit_code, Some(Color::Red), true)
            ),
            JobResult::Killed => format!(
                "Job {id} has been {}",
                style.paint("killed", Some(Color::Red), false)
            ),
            JobResult::CheckpointFailed { .. } => format!(
                "Job {id} was {} after a failed checkpoint",
                style.paint("terminated", Some(Color::Red), false)
            ),
            JobResult::Lost => format!(
                "Job {id} was {}",
                style.paint("lost", Some(Color::Red), false)
            ),
        };
        return format!("{now} - {text}");
    }
    let from = style.paint(state_name(previous), Some(state_color(previous)), false);
    let to = style.paint(state_name(&job.state), Some(state_color(&job.state)), false);
    format!("{now} - Job {id} changed from {from} to {to}")
}

#[cfg(test)]
pub(crate) mod tests;
