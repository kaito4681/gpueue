use gpueue_core::model::{JobResult, JobState};
use gpueue_protocol::api::JobView;

use super::*;
use crate::render::tests::{NOW, ctx, job};

const HOUR: i64 = 3_600_000;

/// Jobs modelled on pueue's query tests.
fn jobs() -> Vec<JobView> {
    let mut failed = job(
        0,
        JobState::Done {
            result: JobResult::Failed { exit_code: 255 },
        },
    );
    failed.label = Some("label-10-0".into());
    failed.started_at = Some(NOW - 2 * HOUR); // 10:00 today
    failed.finished_at = Some(NOW - HOUR);

    let mut success = job(
        1,
        JobState::Done {
            result: JobResult::Success,
        },
    );
    success.label = Some("label-10-1".into());
    success.started_at = Some(NOW - 50 * HOUR); // two days ago
    success.finished_at = Some(NOW - 49 * HOUR);

    let mut stashed = job(2, JobState::Stashed { enqueue_at: None });
    stashed.label = Some("label-10-2".into());

    let mut scheduled = job(
        3,
        JobState::Stashed {
            enqueue_at: Some(NOW + HOUR),
        },
    );
    scheduled.group = "testgroup".into();

    let mut running = job(4, JobState::Running);
    running.started_at = Some(NOW - 100 * HOUR);

    let mut queued = job(5, JobState::Queued { enqueued_at: NOW });
    queued.details.as_mut().unwrap().command = "echo Hello Pueue".into();
    let mut dependent = queued.clone();
    dependent.id = gpueue_core::model::JobId(6);
    dependent.dependencies = vec![queued.id];

    vec![
        failed, success, stashed, scheduled, running, queued, dependent,
    ]
}

fn ids(query: &str) -> Vec<u64> {
    let query = Query::parse(query).unwrap_or_else(|err| panic!("{query}: {err}"));
    query
        .apply(jobs(), &ctx().clock)
        .iter()
        .map(|job| job.id.0)
        .collect()
}

#[test]
fn empty_query_keeps_everything() {
    assert_eq!(ids(""), vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn column_selection() {
    let query = Query::parse("columns=id,status,command").unwrap();
    assert_eq!(
        query.columns,
        vec![Column::Id, Column::Status, Column::Command]
    );
    let query = Query::parse("COLUMNS = id, owner,gpu").unwrap();
    assert_eq!(query.columns, vec![Column::Id, Column::Owner, Column::Gpu]);
    assert!(Query::parse("columns=id,bogus").is_err());
}

#[test]
fn limits() {
    assert_eq!(ids("first 4"), vec![0, 1, 2, 3]);
    assert_eq!(ids("last 4"), vec![3, 4, 5, 6]);
    assert_eq!(ids("first 0"), vec![0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(ids("last 100"), vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn status_filters() {
    assert_eq!(ids("status=queued"), vec![5, 6]);
    assert_eq!(ids("status=running"), vec![4]);
    assert_eq!(ids("status=paused"), Vec::<u64>::new());
    assert_eq!(ids("status=success"), vec![1]);
    assert_eq!(ids("status=failed"), vec![0]);
    assert_eq!(ids("status=stashed"), vec![2, 3]);
    assert_eq!(ids("status!=stashed"), vec![0, 1, 4, 5, 6]);
    assert_eq!(ids("status=Done"), vec![0, 1]);
    assert!(Query::parse("status=bogus").is_err());
    assert!(Query::parse("status%=queued").is_err());
}

#[test]
fn label_filters() {
    assert_eq!(ids("label%=label"), vec![0, 1, 2]);
    assert_eq!(ids("label%=label-10-1"), vec![1]);
    assert_eq!(ids("label=label-10-1"), vec![1]);
    assert_eq!(ids("label!=label-10-1"), vec![0, 2, 3, 4, 5, 6]);
    assert_eq!(ids("label!=label-10"), vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn command_filters() {
    assert_eq!(ids("command=sleep 60"), vec![0, 1, 2, 3, 4]);
    assert_eq!(ids("command!=sleep 60"), vec![5, 6]);
    assert_eq!(ids("command%=echo"), vec![5, 6]);
    assert_eq!(ids("command%=nonexist"), Vec::<u64>::new());
    // As in pueue the value is the rest of the query.
    assert_eq!(ids("command%=echo first 1"), Vec::<u64>::new());
}

#[test]
fn time_filters() {
    // Started after 09:00 today: only the failed job (10:00).
    assert_eq!(ids("start>2026-10-09 09:00:00"), vec![0]);
    assert_eq!(ids("start>2026-10-0909:00"), vec![0]);
    assert_eq!(ids("start > 09:00"), vec![0]);
    // Finished before today.
    assert_eq!(ids("end<2026-10-09"), vec![1]);
    assert_eq!(ids("end=2026-10-09"), vec![0]);
    assert_eq!(ids("end!=2026-10-09"), vec![1]);
    assert_eq!(ids("end>2026-10-08"), vec![0]);
    assert_eq!(ids("enqueue_at>12:30"), vec![3]);
    assert!(Query::parse("start%=2026-10-09").is_err());
    assert!(Query::parse("start>yesterday").is_err());
    // A multi-byte character across the 10th byte is an error, not a panic.
    assert!(Query::parse("start=2023-01-0é").is_err());
    assert!(Query::parse("start=éééééé").is_err());
}

#[test]
fn ordering() {
    assert_eq!(ids("order_by status"), vec![2, 3, 5, 6, 4, 0, 1]);
    assert_eq!(ids("order_by id desc"), vec![6, 5, 4, 3, 2, 1, 0]);
    assert_eq!(ids("order_by enqueue_at asc"), vec![5, 6, 3, 0, 1, 2, 4]);
    assert_eq!(ids("order_by label first 2"), vec![3, 4]);
}

#[test]
fn combined_query() {
    assert_eq!(
        ids("columns=id,status status=queued order_by id desc first 1"),
        vec![6]
    );
    assert!(Query::parse("first 2 status=queued").is_err());
}
