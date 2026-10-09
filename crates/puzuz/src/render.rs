//! Rendering daemon data for humans, modelled on pueue's output.
//!
//! Everything here is pure (data in, `String` out) so it can be tested
//! without a terminal or a daemon.

use std::collections::BTreeMap;

use comfy_table::presets::{NOTHING, UTF8_HORIZONTAL_ONLY};
use comfy_table::{Cell, CellAlignment, ContentArrangement, Row, Table};
use puzuz_core::authz::REDACTED;
use puzuz_core::model::{DEFAULT_GROUP, Group, JobId, JobResult, JobState};
use puzuz_protocol::api::{ActionOutcome, GpuView, JobActionResult, JobView};

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

// ---------------------------------------------------------------------------
// Job states
// ---------------------------------------------------------------------------

/// The state as shown in the status table, with its color (pueue's names
/// and colors, plus the puzuz checkpoint states).
pub fn state_text(job: &JobView) -> (String, Color) {
    match &job.state {
        JobState::Queued { .. } => ("Queued".into(), Color::Yellow),
        JobState::Stashed { .. } => ("Stashed".into(), Color::Yellow),
        JobState::Running => ("Running".into(), Color::Green),
        JobState::Paused => ("Paused".into(), Color::White),
        JobState::Checkpointing => ("Checkpointing".into(), Color::Yellow),
        JobState::Checkpointed => {
            // The last error of a checkpointed job is a failed restore (only
            // in the owner's view; `puzuz log` shows the reason).
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
    let title = style.bold(format!("Group \"{name}\""));
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

/// The output of `puzuz group` (one headline per group).
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
            Column::Label => Cell::new(job.label.as_deref().unwrap_or_default()),
            Column::Owner => Cell::new(ctx.users.name(job.owner)),
            Column::Gpu => Cell::new(
                job.gpus
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Column::Command => match &job.details {
                Some(details) => Cell::new(&details.command),
                None => Cell::new(REDACTED),
            },
            Column::Path => match &job.details {
                Some(details) => Cell::new(details.cwd.to_string_lossy()),
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
                "\nJob list is empty. Add jobs with `puzuz add -g {name} -- [cmd]`"
            )),
        }
        return Ok(out);
    }

    if input.no_jobs_at_all {
        let headline = group_headline(DEFAULT_GROUP, find_group(DEFAULT_GROUP), &ctx.style);
        return Ok(format!(
            "{headline}\n\nJob list is empty. Add jobs with `puzuz add -- [cmd]`"
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

/// The output of `puzuz gpu`.
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
                Cell::new(&gpu.name)
            } else {
                ctx.style.cell(
                    format!("{} (not detected)", gpu.name),
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
    pub fn print(&self) {
        for line in &self.stdout {
            println!("{line}");
        }
        for line in &self.stderr {
            eprintln!("{line}");
        }
    }
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

    if let Some(headline) = headline {
        report.stdout.push(headline);
    } else if !succeeded.is_empty() {
        let ids: Vec<String> = succeeded
            .iter()
            .map(|result| match &result.outcome {
                ActionOutcome::Ok {
                    new_id: Some(new_id),
                } => format!("{} (new id {new_id})", result.id),
                _ => result.id.to_string(),
            })
            .collect();
        report.stdout.push(format!("{message}: {}", ids.join(", ")));
    }

    for result in results {
        if let ActionOutcome::Error { error } = &result.outcome {
            report.ok = false;
            report.stderr.push(style.paint(
                format!("Job {}: {}", result.id, error.message),
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
            JobResult::FailedToSpawn { reason } => {
                (format!("failed to spawn: {reason}"), Color::Red)
            }
            JobResult::Killed => ("killed by system or user".into(), Color::Red),
            JobResult::DependencyFailed => ("dependency failed".into(), Color::Red),
            JobResult::CheckpointFailed { reason } => (
                format!("terminated after a failed checkpoint: {reason}"),
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
        add("Command:", details.command.clone());
        add("Path:", details.cwd.to_string_lossy().into_owned());
    }
    if let Some(label) = &job.label {
        add("Label:", label.clone());
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
        && !state.contains(error)
    {
        let label = match job.state {
            JobState::Checkpointed | JobState::RestoreQueued { .. } | JobState::Restoring => {
                "Restore failed:"
            }
            _ => "Last error:",
        };
        add(label, error.to_owned());
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

fn wait_prefix(ctx: &RenderCtx) -> String {
    ctx.clock.now().format("%H:%M:%S").to_string()
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
