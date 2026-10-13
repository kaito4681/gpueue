//! Executing the subcommands that talk to the daemon.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Local};
use puzuz_core::model::{DEFAULT_GROUP, Group, JobId, JobResult, JobState};
use puzuz_protocol::api::{
    BulkJobRequest, BulkJobResponse, CleanRequest, CreateGroupRequest, EnqueueOptions, ErrorCode,
    Event, JobAction, JobSelector, JobView, KillOptions, ListJobsQuery, LogQuery, PauseOptions,
    ResetRequest, RestartOptions, RestoreOptions, SetGpusParallelRequest, ShutdownRequest,
    StashOptions, SwitchRequest,
};
use puzuz_protocol::client::{Client, ClientError};
use serde::Serialize;

use crate::cli::{GpuCommand, GroupCommand, SubCommand, WaitTargetStatus};
use crate::query::Query;
use crate::render::{self, RenderCtx, StatusInput};
use crate::requests::{self, AddArgs};
use crate::style::Color;

/// Default number of log lines per job (pueue's default).
const DEFAULT_LOG_LINES: usize = 15;

/// `println!` that returns the write error instead of panicking, so that a
/// closed stdout (e.g. `puzuz status | grep -q x`) ends the command quietly.
macro_rules! outln {
    ($($arg:tt)*) => {
        writeln!(io::stdout().lock(), $($arg)*)
    };
}

/// Prints a line, ignoring a closed stdout (for `wait`, whose exit status
/// matters more than its output).
fn print_best_effort(line: &str) {
    let _ = writeln!(io::stdout().lock(), "{line}");
}

/// Why a command failed.
#[derive(Debug)]
pub enum CliError {
    Client(ClientError),
    Message(String),
    /// Whoever read our stdout went away (EPIPE). Not an error worth
    /// reporting: the command stops and exits successfully, like most
    /// command line tools.
    StdoutClosed,
}

impl From<ClientError> for CliError {
    fn from(err: ClientError) -> Self {
        CliError::Client(err)
    }
}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::BrokenPipe {
            CliError::StdoutClosed
        } else {
            CliError::Message(err.to_string())
        }
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Message(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        CliError::Message(message.to_owned())
    }
}

type Result<T, E = CliError> = std::result::Result<T, E>;

/// Exit status of a command that ran to completion.
fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub struct App {
    pub client: Client,
    pub ctx: RenderCtx,
}

impl App {
    /// Runs one subcommand. Commands that do not talk to the daemon are
    /// handled by the caller.
    pub async fn run(&self, command: SubCommand) -> Result<ExitCode> {
        match command {
            SubCommand::Add {
                command,
                working_directory,
                escape,
                immediate,
                follow,
                stashed,
                delay,
                group,
                after,
                priority,
                label,
                print_task_id,
                gpu,
                no_checkpoint,
            } => {
                let cwd = match working_directory {
                    Some(dir) => dir.canonicalize().map_err(|err| {
                        format!("invalid working directory {}: {err}", dir.display())
                    })?,
                    None => std::env::current_dir()?,
                };
                requests::check_working_directory(&cwd)?;
                let args = AddArgs {
                    command: &command,
                    escape,
                    group: group.as_deref(),
                    after: &after,
                    priority,
                    label: label.as_deref(),
                    gpu: &gpu,
                    no_checkpoint,
                    stashed,
                    delay,
                    immediate,
                };
                let request = requests::add_request(&args, cwd, requests::client_environment());
                self.add(request, print_task_id, follow).await
            }
            SubCommand::Remove { task_ids } => self.remove(&task_ids).await,
            SubCommand::Switch {
                task_id_1,
                task_id_2,
            } => {
                self.client
                    .switch(&SwitchRequest {
                        first: JobId(task_id_1 as u64),
                        second: JobId(task_id_2 as u64),
                    })
                    .await?;
                outln!("Jobs have been switched")?;
                Ok(ExitCode::SUCCESS)
            }
            SubCommand::Stash {
                task_ids,
                group,
                all,
                delay,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                let action = JobAction::Stash(StashOptions {
                    enqueue_at: delay.as_ref().map(requests::timestamp),
                });
                let message = match &delay {
                    Some(at) => format!("Jobs are stashed until {}", format_local(at)),
                    None => "Jobs are stashed".to_owned(),
                };
                self.bulk(target, action, &message, None).await
            }
            SubCommand::Enqueue {
                task_ids,
                group,
                all,
                delay,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                let action = JobAction::Enqueue(EnqueueOptions {
                    at: delay.as_ref().map(requests::timestamp),
                });
                let message = match &delay {
                    Some(at) => format!("Jobs will be enqueued at {}", format_local(at)),
                    None => "Jobs are enqueued".to_owned(),
                };
                self.bulk(target, action, &message, None).await
            }
            SubCommand::Start {
                task_ids,
                group,
                all,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                self.group_bulk(target, JobAction::Start, GroupVerb::Start)
                    .await
            }
            SubCommand::Pause {
                task_ids,
                group,
                all,
                wait,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                let action = JobAction::Pause(PauseOptions { wait });
                self.group_bulk(target, action, GroupVerb::Pause { wait })
                    .await
            }
            SubCommand::Kill {
                task_ids,
                group,
                all,
                signal,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                let action = JobAction::Kill(KillOptions {
                    signal: signal.map(|signal| signal.number()),
                });
                let verb = GroupVerb::Kill {
                    signal: signal.map(|signal| signal.to_string()),
                };
                self.group_bulk(target, action, verb).await
            }
            SubCommand::Restart {
                task_ids,
                all_failed,
                failed_in_group,
                immediate,
                stashed,
                in_place,
                not_in_place,
                edit,
            } => {
                if edit {
                    return Err("`restart --edit` is not implemented yet".into());
                }
                let options = RestartOptions {
                    immediate,
                    stashed,
                    in_place: in_place && !not_in_place,
                };
                self.restart(task_ids, all_failed, failed_in_group, options)
                    .await
            }
            SubCommand::Group { json, cmd } => self.group(json, cmd).await,
            SubCommand::Status { query, json, group } => {
                self.status(&query, json, group.as_deref()).await
            }
            SubCommand::Log {
                task_ids,
                group,
                all,
                json,
                lines,
                full,
            } => {
                let lines = if full {
                    None
                } else {
                    Some(lines.unwrap_or(DEFAULT_LOG_LINES))
                };
                self.log(&task_ids, group.as_deref(), all, json, lines)
                    .await
            }
            SubCommand::Follow { task_id, lines } => self.follow(task_id, lines).await,
            SubCommand::Wait {
                task_ids,
                group,
                all,
                quiet,
                status,
            } => {
                let target = requests::selection(&task_ids, group.as_deref(), all);
                self.wait(target, quiet, status.unwrap_or_default()).await
            }
            SubCommand::Clean {
                successful_only,
                group,
            } => {
                let response = self
                    .client
                    .clean(&CleanRequest {
                        successful_only,
                        group,
                    })
                    .await?;
                if response.removed.is_empty() {
                    outln!("No finished jobs to remove.")?;
                } else {
                    let ids: Vec<String> =
                        response.removed.iter().map(ToString::to_string).collect();
                    outln!("Removed finished jobs: {}", ids.join(", "))?;
                }
                Ok(ExitCode::SUCCESS)
            }
            SubCommand::Reset { groups, force } => self.reset(groups, force).await,
            SubCommand::Shutdown { no_checkpoint } => {
                self.client
                    .shutdown(&ShutdownRequest { no_checkpoint })
                    .await?;
                if no_checkpoint {
                    outln!("Daemon is shutting down; running jobs are being killed.")?;
                } else {
                    outln!("Daemon is shutting down; running jobs are being checkpointed.")?;
                }
                Ok(ExitCode::SUCCESS)
            }
            SubCommand::Parallel {
                parallel_tasks,
                group,
            } => match parallel_tasks {
                Some(parallel) => {
                    let group = group.as_deref().unwrap_or(DEFAULT_GROUP);
                    self.client
                        .set_group_parallel(group, to_u32(parallel)?)
                        .await?;
                    outln!("Parallel jobs setting for group \"{group}\" adjusted")?;
                    Ok(ExitCode::SUCCESS)
                }
                None => self.group(false, None).await,
            },
            SubCommand::Checkpoint {
                task_ids,
                group,
                all,
            } => {
                let target = requests::explicit_selection(&task_ids, group.as_deref(), all)
                    .ok_or("specify job ids, --group or --all")?;
                self.bulk(
                    target,
                    JobAction::Checkpoint,
                    "Jobs are being checkpointed",
                    None,
                )
                .await
            }
            SubCommand::Restore {
                task_ids,
                group,
                all,
                immediate,
            } => {
                let target = requests::explicit_selection(&task_ids, group.as_deref(), all)
                    .ok_or("specify job ids, --group or --all")?;
                let action = JobAction::Restore(RestoreOptions { immediate });
                self.bulk(target, action, "Jobs are being restored", None)
                    .await
            }
            SubCommand::Gpu { json, cmd } => self.gpu(json, cmd).await,
            other => Err(format!("`{}` is not implemented yet", other.name()).into()),
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn bulk(
        &self,
        target: JobSelector,
        action: JobAction,
        message: &str,
        headline: Option<String>,
    ) -> Result<ExitCode> {
        let response = self.client.bulk(&BulkJobRequest { target, action }).await?;
        let report = render::action_report(&response.results, message, headline, &self.ctx.style);
        report.print()?;
        Ok(exit(report.ok))
    }

    /// `pause`, `start` and `kill`: the report says whether whole groups
    /// were paused/resumed (administrators) or only the caller's own jobs.
    async fn group_bulk(
        &self,
        target: JobSelector,
        action: JobAction,
        verb: GroupVerb,
    ) -> Result<ExitCode> {
        let response = self
            .client
            .bulk(&BulkJobRequest {
                target: target.clone(),
                action,
            })
            .await?;
        let messages = group_messages(&verb, &target, &response.groups);
        let mut report = render::action_report(
            &response.results,
            &messages.message,
            messages.headline,
            &self.ctx.style,
        );
        report.stdout.extend(messages.note);
        report.print()?;
        Ok(exit(report.ok))
    }

    async fn jobs(&self) -> Result<Vec<JobView>> {
        let mut jobs = self.client.list_jobs(&ListJobsQuery::default()).await?.jobs;
        jobs.sort_by_key(|job| job.id);
        Ok(jobs)
    }

    // -----------------------------------------------------------------------
    // add, remove, restart, reset
    // -----------------------------------------------------------------------

    async fn add(
        &self,
        request: puzuz_protocol::api::AddJobRequest,
        print_task_id: bool,
        follow: bool,
    ) -> Result<ExitCode> {
        let group = request.spec.group.clone();
        let enqueue_at = request.enqueue_at;
        let response = self.client.add_job(&request).await?;
        let id = response.id;

        let mut output = if print_task_id {
            id.to_string()
        } else if let Some(at) = enqueue_at {
            format!(
                "New job added (id {id}). It will be enqueued at {}",
                self.ctx.clock.format_long(at)
            )
        } else {
            format!("New job added (id {id}).")
        };
        if response.launch_pending {
            let note = render::launch_pending_note(id);
            if print_task_id {
                // Keep stdout to the bare id for scripts.
                eprintln!("{note}");
            } else {
                output.push('\n');
                output.push_str(&note);
            }
        }
        if !print_task_id && !follow {
            let groups = self.client.list_groups().await?.groups;
            if groups.iter().any(|g| g.name == group && g.paused) {
                output.push_str("\nThe group of this job is currently paused!");
            }
        }

        if follow {
            // Keep stdout for the job's output.
            eprintln!("{output}");
            return self.follow(Some(id.0 as usize), None).await;
        }
        outln!("{output}")?;
        Ok(ExitCode::SUCCESS)
    }

    async fn remove(&self, task_ids: &[usize]) -> Result<ExitCode> {
        let ids = requests::job_ids(task_ids);
        if io::stdin().is_terminal() {
            let checkpointed: Vec<JobId> = self
                .jobs()
                .await?
                .into_iter()
                .filter(|job| ids.contains(&job.id) && job.state == JobState::Checkpointed)
                .map(|job| job.id)
                .collect();
            if !checkpointed.is_empty()
                && !confirm(
                    "remove checkpointed jobs and delete their checkpoint images",
                    &checkpointed,
                )?
            {
                eprintln!("Aborted!");
                return Ok(ExitCode::FAILURE);
            }
        }
        self.bulk(
            JobSelector::Ids { ids },
            JobAction::Remove,
            "Jobs removed from list",
            None,
        )
        .await
    }

    async fn restart(
        &self,
        task_ids: Vec<usize>,
        all_failed: bool,
        failed_in_group: Option<String>,
        options: RestartOptions,
    ) -> Result<ExitCode> {
        let ids = if all_failed || failed_in_group.is_some() {
            let ids: Vec<JobId> = self
                .jobs()
                .await?
                .into_iter()
                .filter(|job| {
                    !job.is_redacted()
                        && matches!(&job.state, JobState::Done { result } if *result != JobResult::Success)
                        && failed_in_group.as_ref().is_none_or(|group| &job.group == group)
                })
                .map(|job| job.id)
                .collect();
            if ids.is_empty() {
                outln!("No failed jobs to restart.")?;
                return Ok(ExitCode::SUCCESS);
            }
            ids
        } else if task_ids.is_empty() {
            return Err("specify job ids, --all-failed or --failed-in-group".into());
        } else {
            requests::job_ids(&task_ids)
        };
        self.bulk(
            JobSelector::Ids { ids },
            JobAction::Restart(options),
            "Restarted jobs",
            None,
        )
        .await
    }

    async fn reset(&self, groups: Vec<String>, force: bool) -> Result<ExitCode> {
        if !force {
            let running: Vec<JobId> = self
                .jobs()
                .await?
                .into_iter()
                .filter(|job| {
                    !job.is_redacted()
                        && job.state.occupies_slot()
                        && (groups.is_empty() || groups.contains(&job.group))
                })
                .map(|job| job.id)
                .collect();
            if !running.is_empty() && !confirm("remove running jobs", &running)? {
                eprintln!("Aborted!");
                return Ok(ExitCode::FAILURE);
            }
        }

        let requests: Vec<ResetRequest> = if groups.is_empty() {
            vec![ResetRequest { group: None }]
        } else {
            groups
                .iter()
                .map(|group| ResetRequest {
                    group: Some(group.clone()),
                })
                .collect()
        };
        let mut results = Vec::new();
        for request in &requests {
            let BulkJobResponse { results: more, .. } = self.client.reset(request).await?;
            results.extend(more);
        }
        let headline = if groups.is_empty() {
            "Everything is being reset right now.".to_owned()
        } else {
            let names: Vec<String> = groups.iter().map(|g| format!("\"{g}\"")).collect();
            format!("Resetting groups {}.", names.join(", "))
        };
        let report = render::action_report(&results, "", Some(headline), &self.ctx.style);
        report.print()?;
        Ok(exit(report.ok))
    }

    // -----------------------------------------------------------------------
    // group, parallel, gpu
    // -----------------------------------------------------------------------

    async fn group(&self, json: bool, cmd: Option<GroupCommand>) -> Result<ExitCode> {
        match cmd {
            None => {
                let response = self.client.list_groups().await?;
                if json {
                    outln!("{}", to_json(&response))?;
                } else {
                    outln!("{}", render::groups(&response.groups, &self.ctx.style))?;
                }
            }
            Some(GroupCommand::Add { name, parallel }) => {
                let parallel = parallel.map(to_u32).transpose()?;
                self.client
                    .create_group(&CreateGroupRequest {
                        name: name.clone(),
                        parallel,
                    })
                    .await?;
                outln!("New group \"{name}\" has been created")?;
            }
            Some(GroupCommand::Remove { name }) => {
                self.client.delete_group(&name).await?;
                outln!("Group \"{name}\" has been removed")?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }

    async fn gpu(&self, json: bool, cmd: Option<GpuCommand>) -> Result<ExitCode> {
        match cmd {
            None => {
                let response = self.client.list_gpus().await?;
                if json {
                    outln!("{}", to_json(&response))?;
                } else {
                    outln!("{}", render::gpus(&response.gpus, &self.ctx))?;
                }
            }
            Some(GpuCommand::Parallel { parallel_jobs, gpu }) => {
                let parallel = to_u32(parallel_jobs)?;
                let gpus = (!gpu.is_empty()).then(|| requests::gpu_indices(&gpu));
                self.client
                    .set_gpus_parallel(&SetGpusParallelRequest {
                        gpus: gpus.clone(),
                        parallel,
                    })
                    .await?;
                match gpus {
                    Some(gpus) => {
                        let list: Vec<String> = gpus.iter().map(ToString::to_string).collect();
                        outln!(
                            "Parallel jobs setting for GPU {} set to {parallel}",
                            list.join(", ")
                        )?;
                    }
                    None => outln!("Parallel jobs setting for all GPUs set to {parallel}")?,
                }
            }
        }
        Ok(ExitCode::SUCCESS)
    }

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------

    async fn status(&self, query: &[String], json: bool, group: Option<&str>) -> Result<ExitCode> {
        let query = Query::parse(&query.join(" "))?;
        let groups = self.client.list_groups().await?.groups;
        let all_jobs = self.jobs().await?;
        let no_jobs_at_all = all_jobs.is_empty();
        let jobs: Vec<JobView> = all_jobs
            .into_iter()
            .filter(|job| group.is_none_or(|group| job.group == group))
            .collect();
        let jobs = query.apply(jobs, &self.ctx.clock);

        if json {
            outln!("{}", to_json(&StatusJson { groups, jobs }))?;
            return Ok(ExitCode::SUCCESS);
        }
        let output = render::status(
            StatusInput {
                groups: &groups,
                jobs,
                no_jobs_at_all,
                group,
                columns: &query.columns,
            },
            &self.ctx,
        )?;
        outln!("{output}")?;
        Ok(ExitCode::SUCCESS)
    }

    // -----------------------------------------------------------------------
    // log, follow
    // -----------------------------------------------------------------------

    /// Fetches the log of `id`, limited to the last `lines` lines. Also
    /// returns the line limit if output was cut off.
    async fn fetch_log(&self, id: JobId, lines: Option<usize>) -> Result<(Vec<u8>, Option<usize>)> {
        // Ask for one line more than shown to learn whether there is more.
        let query = LogQuery {
            tail: lines.map(|n| n as u64 + 1),
            follow: false,
        };
        let bytes = self.client.job_log(id, &query).await?.collect_all().await?;
        Ok(match lines {
            Some(n) => {
                let (kept, truncated) = render::last_lines(&bytes, n);
                (kept.to_vec(), truncated.then_some(n))
            }
            None => (bytes, None),
        })
    }

    async fn log(
        &self,
        task_ids: &[usize],
        group: Option<&str>,
        all: bool,
        json: bool,
        lines: Option<usize>,
    ) -> Result<ExitCode> {
        let mut ok = true;
        let selection = requests::selection(task_ids, group, all);
        let jobs: Vec<JobView> = match &selection {
            JobSelector::Ids { ids } => {
                let mut jobs = Vec::new();
                for id in ids {
                    match self.client.get_job(*id).await {
                        // Like the group and all selections (which list jobs),
                        // `log --json` leaves out the environment.
                        Ok(job) => jobs.push(job.without_envs()),
                        Err(err @ ClientError::Api { .. }) => {
                            ok = false;
                            eprintln!(
                                "{}",
                                self.ctx.style.paint(
                                    format!("Job {id}: {err}"),
                                    Some(Color::Red),
                                    false
                                )
                            );
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
                jobs
            }
            // Only jobs whose logs the caller may read.
            JobSelector::Group { group } => self
                .jobs()
                .await?
                .into_iter()
                .filter(|job| &job.group == group && !job.is_redacted())
                .collect(),
            JobSelector::All => self
                .jobs()
                .await?
                .into_iter()
                .filter(|job| !job.is_redacted())
                .collect(),
        };
        let jobs: Vec<JobView> = jobs
            .into_iter()
            .filter(|job| render::has_log(&job.state))
            .collect();

        if json {
            let mut out = BTreeMap::new();
            for job in jobs {
                let (bytes, _) = self.fetch_log(job.id, lines).await?;
                out.insert(
                    job.id.to_string(),
                    LogJson {
                        output: String::from_utf8_lossy(&bytes).into_owned(),
                        job,
                    },
                );
            }
            outln!("{}", to_json(&out))?;
            return Ok(exit(ok));
        }

        if jobs.is_empty() {
            match selection {
                JobSelector::Ids { .. } => {
                    eprintln!("There are no started jobs for your specified ids")
                }
                JobSelector::Group { group } => {
                    eprintln!("There are no started jobs in group '{group}'")
                }
                JobSelector::All => eprintln!("There are no started jobs"),
            }
            return Ok(exit(ok));
        }

        let mut stdout = io::stdout();
        for (index, job) in jobs.iter().enumerate() {
            if index > 0 {
                eprintln!();
            }
            eprintln!("{}", render::log_header(job, &self.ctx));
            let (bytes, truncated) = match self.fetch_log(job.id, lines).await {
                Ok(log) => log,
                Err(CliError::Client(err @ ClientError::Api { .. })) => {
                    ok = false;
                    eprintln!(
                        "{}",
                        self.ctx.style.paint(
                            format!("Job {}: {err}", job.id),
                            Some(Color::Red),
                            false
                        )
                    );
                    continue;
                }
                Err(err) => return Err(err),
            };
            if bytes.is_empty() {
                continue;
            }
            eprintln!("\n{}", render::log_output_line(truncated, &self.ctx.style));
            stdout.write_all(&bytes)?;
            stdout.flush()?;
        }
        Ok(exit(ok))
    }

    async fn follow(&self, task_id: Option<usize>, lines: Option<usize>) -> Result<ExitCode> {
        let id = match task_id {
            Some(id) => JobId(id as u64),
            None => {
                let running: Vec<JobId> = self
                    .jobs()
                    .await?
                    .into_iter()
                    .filter(|job| !job.is_redacted() && job.state == JobState::Running)
                    .map(|job| job.id)
                    .collect();
                match running.as_slice() {
                    [] => return Err("There are no running jobs.".into()),
                    [id] => *id,
                    _ => {
                        let ids: Vec<String> = running.iter().map(ToString::to_string).collect();
                        return Err(format!(
                            "Multiple jobs are running, please select one of the following: {}",
                            ids.join(", ")
                        )
                        .into());
                    }
                }
            }
        };

        // Wait for the job to start, as pueue does.
        loop {
            let job = self.client.get_job(id).await?;
            if render::has_log(&job.state) && !matches!(job.state, JobState::RestoreQueued { .. })
                || matches!(job.state, JobState::Done { .. })
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let query = LogQuery {
            tail: lines.map(|n| n as u64),
            follow: true,
        };
        let mut stream = self.client.job_log(id, &query).await?;
        let mut stdout = io::stdout();
        while let Some(chunk) = stream.next_chunk().await? {
            stdout.write_all(&chunk)?;
            stdout.flush()?;
        }
        Ok(ExitCode::SUCCESS)
    }

    // -----------------------------------------------------------------------
    // wait
    // -----------------------------------------------------------------------

    async fn wait(
        &self,
        target: JobSelector,
        quiet: bool,
        target_status: WaitTargetStatus,
    ) -> Result<ExitCode> {
        // Subscribe first so no change between the listing and the stream
        // is missed.
        let mut events = self.client.events().await?;
        let jobs = self.jobs().await?;

        let mut waiter = Waiter::new(target, target_status);
        let mut lines = Vec::new();
        let mut selected = false;
        for job in &jobs {
            if waiter.selects(job) {
                selected = true;
                lines.extend(waiter.add(job, true, &self.ctx));
            }
        }
        // Jobs that already reached the target are not "not found".
        if !selected {
            eprintln!("No jobs found for {}", describe_selection(&waiter.target));
            return Ok(ExitCode::SUCCESS);
        }
        if !quiet {
            for line in &lines {
                print_best_effort(line);
            }
        }

        loop {
            if waiter.failed {
                return Ok(ExitCode::FAILURE);
            }
            if waiter.watched.is_empty() {
                return Ok(ExitCode::SUCCESS);
            }
            let Some(event) = events.next_event().await? else {
                return Err("lost the connection to the daemon while waiting".into());
            };
            for line in waiter.handle(event, &self.ctx)? {
                if !quiet {
                    print_best_effort(&line);
                }
            }
        }
    }
}

/// State of `wait`: the jobs not yet at the target status.
struct Waiter {
    target: JobSelector,
    target_status: WaitTargetStatus,
    watched: HashMap<JobId, JobState>,
    /// With `--status success`: a job finished unsuccessfully.
    failed: bool,
}

impl Waiter {
    fn new(target: JobSelector, target_status: WaitTargetStatus) -> Self {
        Self {
            target,
            target_status,
            watched: HashMap::new(),
            failed: false,
        }
    }

    /// Whether `job` belongs to the selection. For `-g` and `-a` only jobs
    /// the caller may see in full (own jobs, or every job for
    /// administrators) are waited for, matching what bulk actions select.
    fn selects(&self, job: &JobView) -> bool {
        match &self.target {
            JobSelector::Ids { ids } => ids.contains(&job.id),
            JobSelector::Group { group } => &job.group == group && !job.is_redacted(),
            JobSelector::All => !job.is_redacted(),
        }
    }

    /// Starts watching `job` unless it already reached the target.
    fn add(&mut self, job: &JobView, first_run: bool, ctx: &RenderCtx) -> Option<String> {
        let line = render::wait_new_job(job, first_run, ctx);
        if !self.check_reached(job) {
            self.watched.insert(job.id, job.state.clone());
        }
        line
    }

    /// Records whether the job reached the target status (and whether it
    /// failed a `success` wait).
    fn check_reached(&mut self, job: &JobView) -> bool {
        if self.target_status == WaitTargetStatus::Success
            && matches!(&job.state, JobState::Done { result } if *result != JobResult::Success)
        {
            self.failed = true;
        }
        reached(&job.state, self.target_status)
    }

    fn handle(&mut self, event: Event, ctx: &RenderCtx) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        match event {
            Event::JobAdded { job } => {
                // Only `-g`/`-a` waits pick up new jobs, as in pueue.
                if !matches!(self.target, JobSelector::Ids { .. }) && self.selects(&job) {
                    lines.extend(self.add(&job, false, ctx));
                }
            }
            Event::JobUpdated { job } => {
                if let Some(previous) = self.watched.get(&job.id).cloned()
                    && previous != job.state
                {
                    if state_kind_changed(&previous, &job.state) {
                        lines.push(render::wait_state_change(job.id, &previous, &job, ctx));
                    }
                    if self.check_reached(&job) {
                        self.watched.remove(&job.id);
                    } else {
                        self.watched.insert(job.id, job.state.clone());
                    }
                }
            }
            Event::JobRemoved { id } => {
                self.watched.remove(&id);
            }
            Event::DaemonShutdown => {
                return Err("the daemon is shutting down".into());
            }
            Event::GroupUpdated { .. } | Event::GroupRemoved { .. } => {}
        }
        Ok(lines)
    }
}

/// Whether the state changed in a way worth reporting (not just a
/// timestamp inside the same state).
fn state_kind_changed(previous: &JobState, next: &JobState) -> bool {
    std::mem::discriminant(previous) != std::mem::discriminant(next)
        || matches!(next, JobState::Done { .. }) && previous != next
}

/// A bulk action that may also pause or resume whole groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupVerb {
    Pause {
        wait: bool,
    },
    Start,
    /// The signal's name, if one is sent instead of killing.
    Kill {
        signal: Option<String>,
    },
}

/// What [`App::group_bulk`] prints: `message` prefixes the list of
/// affected jobs unless there is a `headline`; `note` follows the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessages {
    pub message: String,
    pub headline: Option<String>,
    pub note: Option<String>,
}

/// The messages for `verb` on `target`, given the groups whose paused flag
/// the daemon set (`BulkJobResponse::groups`). Only administrators change a
/// group; for everyone else `-g`/`-a` only act on their own jobs, which the
/// wording reflects.
pub fn group_messages(verb: &GroupVerb, target: &JobSelector, groups: &[String]) -> GroupMessages {
    let changed = match target {
        JobSelector::Ids { .. } => false,
        JobSelector::Group { group } => groups.contains(group),
        JobSelector::All => !groups.is_empty(),
    };
    let jobs_of = match target {
        JobSelector::Group { group } => format!(" in group \"{group}\""),
        _ => String::new(),
    };
    let own = !changed && !matches!(target, JobSelector::Ids { .. });
    let (message, headline, note) = match verb {
        GroupVerb::Pause { wait } => {
            let suffix = if *wait {
                " Running jobs keep running."
            } else {
                ""
            };
            let headline = match target {
                JobSelector::Group { group } if changed => {
                    Some(format!("Group \"{group}\" is being paused.{suffix}"))
                }
                JobSelector::All if changed => {
                    Some(format!("All groups are being paused.{suffix}"))
                }
                // `--wait` only pauses groups, which is up to administrators.
                _ if own && *wait => {
                    Some("Only administrators pause groups; no jobs were paused.".to_owned())
                }
                _ => None,
            };
            let message = if own {
                format!("Your jobs{jobs_of} are being paused")
            } else {
                "Jobs are being paused".to_owned()
            };
            (message, headline, None)
        }
        GroupVerb::Start => {
            let headline = match target {
                JobSelector::Group { group } if changed => {
                    Some(format!("Group \"{group}\" is being resumed."))
                }
                JobSelector::All if changed => Some("All groups are being resumed.".to_owned()),
                _ => None,
            };
            let message = if own {
                format!("Your paused jobs{jobs_of} are being resumed")
            } else {
                "Jobs are being started".to_owned()
            };
            (message, headline, None)
        }
        GroupVerb::Kill { signal } => {
            let message = match (signal, own) {
                (Some(signal), true) => format!("Sending signal {signal} to your jobs{jobs_of}"),
                (Some(signal), false) => format!("Sending signal {signal} to jobs"),
                (None, true) => format!("Your jobs{jobs_of} are being killed"),
                (None, false) => "Jobs are being killed".to_owned(),
            };
            let note = match target {
                JobSelector::Group { group } if changed => {
                    Some(format!("Group \"{group}\" has been paused."))
                }
                JobSelector::All if changed => Some("All groups have been paused.".to_owned()),
                _ => None,
            };
            (message, None, note)
        }
    };
    GroupMessages {
        message,
        headline,
        note,
    }
}

/// Whether a job in `state` satisfies `wait --status target`.
pub fn reached(state: &JobState, target: WaitTargetStatus) -> bool {
    let done = matches!(state, JobState::Done { .. });
    match target {
        WaitTargetStatus::Done => done,
        WaitTargetStatus::Success => matches!(
            state,
            JobState::Done {
                result: JobResult::Success
            }
        ),
        // Every state a job can only reach after it was queued.
        WaitTargetStatus::Queued => !matches!(state, JobState::Stashed { .. }),
        // Every state a job can only reach after it started.
        WaitTargetStatus::Running => {
            !matches!(state, JobState::Stashed { .. } | JobState::Queued { .. })
        }
    }
}

fn describe_selection(target: &JobSelector) -> String {
    match target {
        JobSelector::Ids { ids } => {
            let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
            format!("ids {}", ids.join(", "))
        }
        JobSelector::Group { group } => format!("group \"{group}\""),
        JobSelector::All => "all groups".to_owned(),
    }
}

/// The JSON printed by `status --json`:
/// `{"groups": [Group, ...], "jobs": [JobView, ...]}`, where `jobs` is
/// filtered by `--group` and the query. The views are those of
/// `GET /v1/jobs`, so they carry no `envs`.
#[derive(Serialize)]
pub struct StatusJson {
    pub groups: Vec<Group>,
    pub jobs: Vec<JobView>,
}

/// One entry of `log --json`, keyed by job id:
/// `{"<id>": {"job": JobView, "output": "..."}}`. The views carry no
/// `envs`, whichever way the jobs were selected.
#[derive(Serialize)]
struct LogJson {
    job: JobView,
    output: String,
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("output serializes")
}

fn to_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| CliError::Message(format!("{value} is too large")))
}

fn format_local(at: &DateTime<Local>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Asks `You are trying to {action}: 1, 2` and reads a `[Y/n]` answer from
/// stdin, like pueue. End of input counts as "no".
fn confirm(action: &str, ids: &[JobId]) -> Result<bool> {
    let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
    eprintln!("You are trying to {action}: {}", ids.join(", "));
    let stdin = io::stdin();
    loop {
        eprint!("Do you want to continue [Y/n]: ");
        io::stderr().flush()?;
        let mut input = String::new();
        if stdin.lock().read_line(&mut input)? == 0 {
            return Ok(false);
        }
        match input.trim() {
            "" | "y" | "Y" | "yes" => return Ok(true),
            "n" | "N" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// The message shown for a failed command; daemon errors get hints.
pub async fn describe_error(err: CliError, client: &Client) -> String {
    let err = match err {
        CliError::Message(message) => return message,
        CliError::StdoutClosed => return "standard output was closed".to_owned(),
        CliError::Client(err) => err,
    };
    if err.is_daemon_down() {
        return format!(
            "the daemon is not running ({err}).\n\
             Check `puzuz service status` or `systemctl status puzuz`."
        );
    }
    if let ClientError::Connect { source, .. } = &err
        && source.kind() == io::ErrorKind::PermissionDenied
    {
        return format!("{err}.\nThe socket is not accessible to your user.");
    }
    if err.code() == Some(ErrorCode::VersionMismatch) {
        let daemon = match client.daemon_version().await {
            Ok(version) => format!("daemon {}", version.daemon_version),
            Err(_) => "the daemon".to_owned(),
        };
        // A CLI from the Nix store that is not the NixOS system's own.
        let exe = std::env::current_exe().ok().filter(|exe| {
            exe.starts_with("/nix/store")
                && std::fs::canonicalize(NIXOS_SYSTEM_CLI).ok().as_ref() != Some(exe)
        });
        return version_mismatch_message(&err, client.version(), &daemon, exe.as_deref());
    }
    err.to_string()
}

/// The CLI of the NixOS system (`services.puzuz`), whose version follows the daemon's.
const NIXOS_SYSTEM_CLI: &str = "/run/current-system/sw/bin/puzuz";

/// The hint for a CLI whose version differs from the daemon's. `nix_exe` is set for a CLI run
/// from the Nix store other than the NixOS system's (home-manager, `nix profile`, `nix run`):
/// it usually comes from another flake revision than the daemon, so it is told how to use the
/// system's CLI instead.
fn version_mismatch_message(
    err: &ClientError,
    ours: &str,
    daemon: &str,
    nix_exe: Option<&std::path::Path>,
) -> String {
    match nix_exe {
        Some(exe) => format!(
            "{err}\nThis puzuz is {ours} from {} but talks to {daemon}. A puzuz from \
             home-manager or `nix profile` must have the daemon's version: pin it to the \
             system's version, or set `programs.puzuz.package = null` (home-manager) to use \
             the system's puzuz.",
            exe.display()
        ),
        None => format!(
            "{err}\nThis client is puzuz {ours} but talks to {daemon}. If the daemon is \
             outdated, an administrator can restart it with `sudo systemctl restart puzuz` \
             (running jobs keep running); otherwise update this client."
        ),
    }
}

#[cfg(test)]
mod tests {
    use puzuz_core::model::{JobResult, Uid};
    use puzuz_protocol::api::ApiError;

    use super::*;
    use crate::render::tests::{ctx, job};

    #[test]
    fn version_mismatch_hints_at_the_install() {
        let err = ClientError::Api {
            status: 409,
            error: ApiError::new(ErrorCode::VersionMismatch, "version mismatch"),
        };
        let exe = std::path::Path::new("/nix/store/abc-puzuz-0.3.0/bin/puzuz");
        let nix = version_mismatch_message(&err, "0.3.0", "daemon 0.2.0", Some(exe));
        assert!(
            nix.contains("0.3.0 from /nix/store/abc-puzuz-0.3.0/bin/puzuz"),
            "{nix}"
        );
        assert!(nix.contains("programs.puzuz.package = null"), "{nix}");
        let other = version_mismatch_message(&err, "0.3.0", "daemon 0.2.0", None);
        assert!(other.contains("sudo systemctl restart puzuz"), "{other}");
        assert!(!other.contains("home-manager"), "{other}");
    }

    #[test]
    fn broken_pipe_is_not_an_error_message() {
        let err = CliError::from(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(matches!(err, CliError::StdoutClosed));
        let err = CliError::from(io::Error::other("disk full"));
        assert!(matches!(err, CliError::Message(m) if m == "disk full"));
    }

    #[test]
    fn wait_targets() {
        let queued = JobState::Queued { enqueued_at: 0 };
        let stashed = JobState::Stashed { enqueue_at: None };
        let failed = JobState::Done {
            result: JobResult::Failed { exit_code: 1 },
        };
        let success = JobState::Done {
            result: JobResult::Success,
        };
        assert!(!reached(&stashed, WaitTargetStatus::Queued));
        assert!(reached(&queued, WaitTargetStatus::Queued));
        assert!(!reached(&queued, WaitTargetStatus::Running));
        assert!(reached(&JobState::Checkpointed, WaitTargetStatus::Running));
        assert!(!reached(&JobState::Checkpointed, WaitTargetStatus::Done));
        assert!(reached(&failed, WaitTargetStatus::Done));
        assert!(!reached(&failed, WaitTargetStatus::Success));
        assert!(reached(&success, WaitTargetStatus::Success));
        assert!(reached(&success, WaitTargetStatus::Done));
    }

    #[test]
    fn waiter_follows_events_until_done() {
        let ctx = ctx();
        let mut waiter = Waiter::new(
            JobSelector::Group {
                group: "default".into(),
            },
            WaitTargetStatus::Done,
        );
        let mut running = job(1, JobState::Running);
        assert!(waiter.selects(&running));
        let line = waiter.add(&running, true, &ctx).unwrap();
        assert_eq!(line, "12:00:00 - Found active job 1 with status Running");
        assert_eq!(waiter.watched.len(), 1);

        // Another user's job in the group is not waited for.
        let mut foreign = job(2, JobState::Running);
        foreign.owner = Uid(2000);
        foreign.details = None;
        assert!(!waiter.selects(&foreign));

        // A new job in the group is picked up.
        let queued = job(3, JobState::Queued { enqueued_at: 0 });
        let lines = waiter
            .handle(
                Event::JobAdded {
                    job: queued.clone(),
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(lines, vec!["12:00:00 - New job 3 with status Queued"]);

        running.state = JobState::Done {
            result: JobResult::Failed { exit_code: 2 },
        };
        let lines = waiter
            .handle(Event::JobUpdated { job: running }, &ctx)
            .unwrap();
        assert_eq!(lines, vec!["12:00:00 - Job 1 failed with 2"]);
        assert_eq!(waiter.watched.len(), 1);
        assert!(!waiter.failed);

        let lines = waiter
            .handle(Event::JobRemoved { id: JobId(3) }, &ctx)
            .unwrap();
        assert!(lines.is_empty());
        assert!(waiter.watched.is_empty());
    }

    #[test]
    fn success_wait_fails_on_failed_job() {
        let ctx = ctx();
        let mut waiter = Waiter::new(
            JobSelector::Ids {
                ids: vec![JobId(1)],
            },
            WaitTargetStatus::Success,
        );
        let mut running = job(1, JobState::Running);
        waiter.add(&running, true, &ctx);
        running.state = JobState::Done {
            result: JobResult::Killed,
        };
        let lines = waiter
            .handle(Event::JobUpdated { job: running }, &ctx)
            .unwrap();
        assert_eq!(lines, vec!["12:00:00 - Job 1 has been killed"]);
        assert!(waiter.failed);
    }
}
