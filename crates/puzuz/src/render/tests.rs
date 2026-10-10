//! Snapshot-style tests of the rendered output (colors disabled, no
//! terminal width, fixed clock and user names).

use std::path::PathBuf;

use chrono::DateTime;
use puzuz_core::model::{GpuIndex, Group, JobId, JobResult, JobState, Uid};
use puzuz_protocol::api::{
    ActionOutcome, ApiError, ErrorCode, GpuJob, GpuView, JobActionResult, JobDetails, JobView,
};

use super::*;
use crate::clock::Clock;
use crate::style::Style;
use crate::users::UserNames;

/// 2026-10-09 12:00:00 UTC.
pub(crate) const NOW: i64 = 1_791_547_200_000;
const HOUR: i64 = 3_600_000;

pub(crate) fn ctx() -> RenderCtx {
    RenderCtx {
        style: Style::PLAIN,
        clock: Clock::fixed(DateTime::from_timestamp_millis(NOW).unwrap().fixed_offset()),
        users: UserNames::fixed(&[(1000, "alice"), (2000, "bob")]),
        detect_width: false,
    }
}

/// A job of alice in the default group.
pub(crate) fn job(id: u64, state: JobState) -> JobView {
    JobView {
        id: JobId(id),
        owner: Uid(1000),
        group: "default".into(),
        gpus: vec![],
        dependencies: vec![],
        priority: 0,
        label: None,
        no_checkpoint: false,
        state,
        created_at: NOW - 3 * HOUR,
        started_at: None,
        finished_at: None,
        checkpoint_size_bytes: None,
        details: Some(JobDetails {
            command: "sleep 60".into(),
            cwd: PathBuf::from("/tmp"),
            envs: None,
            last_error: None,
        }),
    }
}

fn lines(lines: &[&str]) -> String {
    lines.join("\n")
}

fn group(name: &str, parallel: u32, paused: bool) -> Group {
    Group {
        name: name.into(),
        parallel,
        paused,
    }
}

fn sample_jobs() -> Vec<JobView> {
    let mut done = job(
        0,
        JobState::Done {
            result: JobResult::Success,
        },
    );
    done.started_at = Some(NOW - 2 * HOUR);
    done.finished_at = Some(NOW - HOUR);

    let mut failed_yesterday = job(
        1,
        JobState::Done {
            result: JobResult::Failed { exit_code: 3 },
        },
    );
    failed_yesterday.started_at = Some(NOW - 26 * HOUR);
    failed_yesterday.finished_at = Some(NOW - 25 * HOUR);

    let mut running = job(2, JobState::Running);
    running.started_at = Some(NOW - HOUR);
    running.gpus = vec![GpuIndex(0), GpuIndex(1)];

    let mut foreign = job(3, JobState::Checkpointed);
    foreign.owner = Uid(2000);
    foreign.details = None;
    foreign.checkpoint_size_bytes = Some(3 * 1024 * 1024 * 1024);
    foreign.started_at = Some(NOW - 2 * HOUR);
    foreign.gpus = vec![GpuIndex(1)];

    let mut queued = job(4, JobState::Queued { enqueued_at: NOW });
    queued.group = "cpu".into();

    vec![done, failed_yesterday, running, foreign, queued]
}

fn groups() -> Vec<Group> {
    vec![group("default", 2, false), group("cpu", 1, true)]
}

#[test]
fn status_renders_one_table_per_group() {
    let groups = groups();
    let output = status(
        StatusInput {
            groups: &groups,
            jobs: sample_jobs(),
            no_jobs_at_all: false,
            group: None,
            columns: &[],
        },
        &ctx(),
    )
    .unwrap();
    let expected = lines(&[
        r#"Group "default" (2 parallel): running"#,
        "───────────────────────────────────────────────────────────────────────────────────────────────",
        " Id   Status                   Owner   GPU   Command      Path         Start        End        ",
        "═══════════════════════════════════════════════════════════════════════════════════════════════",
        " 0    Success                  alice         sleep 60     /tmp         10:00:00     11:00:00   ",
        "───────────────────────────────────────────────────────────────────────────────────────────────",
        " 1    Failed (3)               alice         sleep 60     /tmp         2026-10-08   2026-10-08 ",
        "                                                                       10:00:00     11:00:00   ",
        "───────────────────────────────────────────────────────────────────────────────────────────────",
        " 2    Running                  alice   0,1   sleep 60     /tmp         11:00:00                ",
        "───────────────────────────────────────────────────────────────────────────────────────────────",
        " 3    Checkpointed (3.0 GiB)   bob     1     <redacted>   <redacted>   10:00:00                ",
        "───────────────────────────────────────────────────────────────────────────────────────────────",
        "",
        r#"Group "cpu" (1 parallel): paused"#,
        "─────────────────────────────────────────────────────",
        " Id   Status   Owner   Command    Path   Start   End ",
        "═════════════════════════════════════════════════════",
        " 4    Queued   alice   sleep 60   /tmp               ",
        "─────────────────────────────────────────────────────",
    ]);
    assert_eq!(output, expected, "\n{output}");
}

#[test]
fn status_for_empty_daemon_shows_hint() {
    let groups = groups();
    let output = status(
        StatusInput {
            groups: &groups,
            jobs: vec![],
            no_jobs_at_all: true,
            group: None,
            columns: &[],
        },
        &ctx(),
    )
    .unwrap();
    assert_eq!(
        output,
        "Group \"default\" (2 parallel): running\n\n\
         Job list is empty. Add jobs with `puzuz add -- [cmd]`"
    );
}

#[test]
fn status_for_single_group() {
    let groups = groups();
    let render = |jobs: Vec<JobView>, name: &str| {
        status(
            StatusInput {
                groups: &groups,
                jobs,
                no_jobs_at_all: false,
                group: Some(name),
                columns: &[],
            },
            &ctx(),
        )
    };
    assert_eq!(
        render(vec![], "cpu").unwrap(),
        "Group \"cpu\" (1 parallel): paused\n\
         Job list is empty. Add jobs with `puzuz add -g cpu -- [cmd]`"
    );
    let output = render(vec![sample_jobs().remove(4)], "cpu").unwrap();
    let expected = lines(&[
        r#"Group "cpu" (1 parallel): paused"#,
        "─────────────────────────────────────────────────────",
        " Id   Status   Owner   Command    Path   Start   End ",
        "═════════════════════════════════════════════════════",
        " 4    Queued   alice   sleep 60   /tmp               ",
        "─────────────────────────────────────────────────────",
    ]);
    assert_eq!(output, expected, "\n{output}");
    assert_eq!(
        render(vec![], "nope").unwrap_err(),
        "There exists no group \"nope\""
    );
}

#[test]
fn status_with_selected_columns_and_optional_columns() {
    let groups = groups();
    let mut labelled = job(
        7,
        JobState::Stashed {
            enqueue_at: Some(NOW + HOUR),
        },
    );
    labelled.label = Some("exp-1".into());
    labelled.priority = 5;
    labelled.dependencies = vec![JobId(1), JobId(2)];
    let output = status(
        StatusInput {
            groups: &groups,
            jobs: vec![labelled.clone()],
            no_jobs_at_all: false,
            group: None,
            columns: &[],
        },
        &ctx(),
    )
    .unwrap();
    let expected = lines(&[
        r#"Group "default" (2 parallel): running"#,
        "─────────────────────────────────────────────────────────────────────────────────────────",
        " Id   Status    Prio   Enqueue At   Deps   Label   Owner   Command    Path   Start   End ",
        "═════════════════════════════════════════════════════════════════════════════════════════",
        " 7    Stashed   5      13:00:00     1, 2   exp-1   alice   sleep 60   /tmp               ",
        "─────────────────────────────────────────────────────────────────────────────────────────",
    ]);
    assert_eq!(output, expected, "\n{output}");

    let output = status(
        StatusInput {
            groups: &groups,
            jobs: vec![labelled],
            no_jobs_at_all: false,
            group: None,
            columns: &[Column::Id, Column::Label],
        },
        &ctx(),
    )
    .unwrap();
    let expected = lines(&[
        r#"Group "default" (2 parallel): running"#,
        "────────────",
        " Id   Label ",
        "════════════",
        " 7    exp-1 ",
        "────────────",
    ]);
    assert_eq!(output, expected, "\n{output}");
}

#[test]
fn group_list() {
    assert_eq!(
        groups_text(),
        "Group \"default\" (2 parallel): running\nGroup \"cpu\" (1 parallel): paused"
    );
}

fn groups_text() -> String {
    super::groups(&groups(), &Style::PLAIN)
}

#[test]
fn gpu_table() {
    let gpus = vec![
        GpuView {
            index: GpuIndex(0),
            uuid: "GPU-0".into(),
            name: "NVIDIA A100".into(),
            memory_total_bytes: Some(80 * 1024 * 1024 * 1024),
            driver_version: None,
            available: true,
            parallel: 1,
            used: 1,
            jobs: vec![GpuJob {
                id: JobId(2),
                owner: Uid(1000),
            }],
        },
        GpuView {
            index: GpuIndex(1),
            uuid: "GPU-1".into(),
            name: "NVIDIA A100".into(),
            memory_total_bytes: None,
            driver_version: None,
            available: true,
            parallel: 0,
            used: 2,
            jobs: vec![
                GpuJob {
                    id: JobId(2),
                    owner: Uid(1000),
                },
                GpuJob {
                    id: JobId(3),
                    owner: Uid(2000),
                },
            ],
        },
    ];
    let output = super::gpus(&gpus, &ctx());
    let expected = lines(&[
        "─────────────────────────────────────────────────────────────────",
        " GPU   Name          Memory     Slots         Jobs               ",
        "═════════════════════════════════════════════════════════════════",
        " 0     NVIDIA A100   80.0 GiB   1/1           2 (alice)          ",
        "─────────────────────────────────────────────────────────────────",
        " 1     NVIDIA A100   -          2/unlimited   2 (alice), 3 (bob) ",
        "─────────────────────────────────────────────────────────────────",
    ]);
    assert_eq!(output, expected, "\n{output}");
    assert_eq!(super::gpus(&[], &ctx()), "No GPUs detected.");
}

#[test]
fn action_report_lists_successes_and_failures() {
    let results = vec![
        JobActionResult::ok(JobId(1)),
        JobActionResult {
            id: JobId(2),
            outcome: ActionOutcome::Ok {
                new_id: Some(JobId(9)),
            },
        },
        JobActionResult::error(
            JobId(3),
            ApiError::new(ErrorCode::InvalidState, "job 3 is not running"),
        ),
    ];
    let report = action_report(&results, "Jobs are being killed", None, &Style::PLAIN);
    assert_eq!(
        report,
        Report {
            stdout: vec!["Jobs are being killed: 1, 2 (new id 9)".into()],
            stderr: vec!["Job 3: job 3 is not running".into()],
            ok: false,
        }
    );

    let report = action_report(
        &[],
        "Jobs are being paused",
        Some("Group \"default\" is being paused.".into()),
        &Style::PLAIN,
    );
    assert_eq!(report.stdout, vec!["Group \"default\" is being paused."]);
    assert!(report.ok);

    let report = action_report(&[], "Jobs are being killed", None, &Style::PLAIN);
    assert_eq!(report.stdout, vec!["No matching jobs."]);
}

#[test]
fn log_header_shows_job_details() {
    let mut finished = job(
        5,
        JobState::Done {
            result: JobResult::Failed { exit_code: 1 },
        },
    );
    finished.label = Some("exp".into());
    finished.started_at = Some(NOW - HOUR);
    finished.finished_at = Some(NOW);
    let output = log_header(&finished, &ctx());
    let expected = lines(&[
        "┌───────────────────────────────────┐",
        "│ Job 5:    failed with exit code 1 │",
        "└───────────────────────────────────┘",
        "Command: sleep 60            ",
        "   Path: /tmp                ",
        "  Label: exp                 ",
        "  Owner: alice               ",
        "  Start: 2026-10-09 11:00:00 ",
        "    End: 2026-10-09 12:00:00 ",
    ]);
    assert_eq!(output, expected, "\n{output}");
    assert_eq!(
        log_output_line(Some(15), &Style::PLAIN),
        "output: (last 15 lines)"
    );
}

fn with_error(mut job: JobView, error: &str) -> JobView {
    job.details.as_mut().unwrap().last_error = Some(error.into());
    job
}

#[test]
fn failed_restore_is_shown_in_status_and_log_header() {
    let mut checkpointed = with_error(
        job(6, JobState::Checkpointed),
        "criu restore failed (unit inactive)",
    );
    checkpointed.checkpoint_size_bytes = Some(4096);
    assert_eq!(
        state_text(&checkpointed),
        (
            "Checkpointed (4.0 KiB, restore failed)".to_owned(),
            Color::Red
        )
    );
    let mut no_size = checkpointed.clone();
    no_size.checkpoint_size_bytes = None;
    assert_eq!(state_text(&no_size).0, "Checkpointed (restore failed)");

    let output = log_header(&checkpointed, &ctx());
    let expected = lines(&[
        "┌────────────────────────┐",
        "│ Job 6:    checkpointed │",
        "└────────────────────────┘",
        "       Command: sleep 60                            ",
        "          Path: /tmp                                ",
        "         Owner: alice                               ",
        "Restore failed: criu restore failed (unit inactive) ",
    ]);
    assert_eq!(output, expected, "\n{output}");

    // A redacted view has no error to show.
    let mut foreign = checkpointed.clone();
    foreign.details = None;
    assert_eq!(state_text(&foreign).0, "Checkpointed (4.0 KiB)");
    assert!(!log_header(&foreign, &ctx()).contains("criu"));
}

#[test]
fn log_header_does_not_repeat_the_reason_of_the_result() {
    let spawn = with_error(
        job(
            7,
            JobState::Done {
                result: JobResult::FailedToSpawn {
                    reason: "no such directory".into(),
                },
            },
        ),
        "no such directory",
    );
    let output = log_header(&spawn, &ctx());
    assert_eq!(output.matches("no such directory").count(), 1, "\n{output}");
    assert!(!output.contains("Last error:"), "\n{output}");

    // A failed dump of a job that kept running.
    let running = with_error(job(8, JobState::Running), "criu: tcp connection");
    let output = log_header(&running, &ctx());
    assert!(
        output.contains("Last error: criu: tcp connection"),
        "\n{output}"
    );
}

#[test]
fn last_lines_keeps_the_tail() {
    assert_eq!(last_lines(b"a\nb\nc\n", 2), (&b"b\nc\n"[..], true));
    assert_eq!(last_lines(b"a\nb\nc", 2), (&b"b\nc"[..], true));
    assert_eq!(last_lines(b"a\nb\n", 2), (&b"a\nb\n"[..], false));
    assert_eq!(last_lines(b"", 2), (&b""[..], false));
    assert_eq!(last_lines(b"a\n", 0), (&b""[..], true));
}

#[test]
fn state_texts() {
    let texts: Vec<String> = sample_jobs().iter().map(|j| state_text(j).0).collect();
    assert_eq!(
        texts,
        vec![
            "Success",
            "Failed (3)",
            "Running",
            "Checkpointed (3.0 GiB)",
            "Queued"
        ]
    );
}
