//! Golden and round-trip tests of the CLI ⇄ daemon wire format.
//!
//! The golden strings pin the exact JSON shape. If one of these fails, the
//! wire format changed: make sure that is intended and compatible.

use std::collections::BTreeMap;
use std::fmt::Debug;

use puzuz_core::model::{Caller, GpuIndex, Group, Job, JobId, JobResult, JobSpec, JobState, Uid};
use puzuz_protocol::api::routes::{self, JobActionKind};
use puzuz_protocol::api::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn assert_golden<T>(value: &T, golden: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let json = serde_json::to_string(value).unwrap();
    assert_eq!(json, golden);
    let back: T = serde_json::from_str(golden).unwrap();
    assert_eq!(&back, value);
}

fn sample_job() -> Job {
    Job {
        id: JobId(7),
        owner: Uid(1000),
        spec: JobSpec {
            command: "python train.py --secret-token abc".into(),
            cwd: "/home/alice/proj".into(),
            envs: BTreeMap::from([("WANDB_API_KEY".into(), "k".into())]),
            group: "default".into(),
            gpus: vec![GpuIndex(0), GpuIndex(1)],
            dependencies: vec![],
            priority: 0,
            label: Some("run1".into()),
            no_checkpoint: false,
        },
        state: JobState::Running,
        created_at: 1_000,
        started_at: Some(2_000),
        finished_at: None,
    }
}

#[test]
fn api_error_golden() {
    let err = ApiError::new(ErrorCode::PermissionDenied, "job 7 belongs to uid 1000");
    assert_golden(
        &err,
        r#"{"code":"permission_denied","message":"job 7 belongs to uid 1000"}"#,
    );
    assert_eq!(err.http_status(), 403);
}

#[test]
fn error_from_core() {
    let err = ApiError::from(puzuz_core::Error::JobNotFound(JobId(3)));
    assert_eq!(err.code, ErrorCode::JobNotFound);
    assert_eq!(err.http_status(), 404);
}

#[test]
fn redacted_view_hides_private_fields() {
    let job = sample_job();
    let stranger = Caller {
        uid: Uid(1001),
        is_admin: false,
    };
    let view = JobView::for_caller(&job, &stranger);
    assert!(view.is_redacted());
    let json = serde_json::to_string(&view).unwrap();
    assert_eq!(
        json,
        r#"{"id":7,"owner":1000,"group":"default","gpus":[0,1],"dependencies":[],"priority":0,"label":null,"no_checkpoint":false,"state":{"state":"running"},"created_at":1000,"started_at":2000,"finished_at":null}"#
    );
    for secret in [
        "run1",
        "train.py",
        "/home/alice",
        "WANDB_API_KEY",
        "command",
        "cwd",
        "envs",
    ] {
        assert!(!json.contains(secret), "leaked {secret}: {json}");
    }
}

#[test]
fn full_view_for_owner_and_admin() {
    let job = sample_job();
    let owner = Caller {
        uid: Uid(1000),
        is_admin: false,
    };
    let admin = Caller {
        uid: Uid(0),
        is_admin: true,
    };
    for caller in [owner, admin] {
        let view = JobView::for_caller(&job, &caller);
        let details = view.details.as_ref().expect("full view");
        assert_eq!(details.command, job.spec.command);
        assert_eq!(details.envs, job.spec.envs);
    }
    let view = JobView::full(&job);
    let json = serde_json::to_string(&view).unwrap();
    assert!(json.ends_with(
        r#""details":{"command":"python train.py --secret-token abc","cwd":"/home/alice/proj","envs":{"WANDB_API_KEY":"k"}}}"#
    ));
    let back: JobView = serde_json::from_str(&json).unwrap();
    assert_eq!(back, view);
}

#[test]
fn add_job_request_golden() {
    let req = AddJobRequest {
        spec: JobSpec {
            command: "echo hi".into(),
            cwd: "/tmp".into(),
            envs: BTreeMap::new(),
            group: "default".into(),
            gpus: vec![GpuIndex(0)],
            dependencies: vec![JobId(1)],
            priority: 0,
            label: None,
            no_checkpoint: true,
        },
        stashed: false,
        enqueue_at: None,
        immediate: false,
    };
    assert_golden(
        &req,
        r#"{"command":"echo hi","cwd":"/tmp","envs":{},"group":"default","gpus":[0],"dependencies":[1],"priority":0,"label":null,"no_checkpoint":true,"stashed":false,"immediate":false}"#,
    );
    // Optional flags may be omitted by hand-written clients (curl).
    let minimal: AddJobRequest = serde_json::from_str(
        r#"{"command":"echo hi","cwd":"/tmp","envs":{},"group":"default","gpus":[0],"dependencies":[1],"priority":0,"label":null,"no_checkpoint":true}"#,
    )
    .unwrap();
    assert_eq!(minimal, req);
}

#[test]
fn bulk_request_golden() {
    let req = BulkJobRequest {
        target: JobSelector::Ids {
            ids: vec![JobId(1), JobId(2)],
        },
        action: JobAction::Kill(KillOptions { signal: Some(9) }),
    };
    assert_golden(
        &req,
        r#"{"target":{"kind":"ids","ids":[1,2]},"action":"kill","signal":9}"#,
    );

    let req = BulkJobRequest {
        target: JobSelector::All,
        action: JobAction::Checkpoint,
    };
    assert_golden(&req, r#"{"target":{"kind":"all"},"action":"checkpoint"}"#);

    let req = BulkJobRequest {
        target: JobSelector::Group {
            group: "gpu".into(),
        },
        action: JobAction::Restore(RestoreOptions { immediate: true }),
    };
    assert_golden(
        &req,
        r#"{"target":{"kind":"group","group":"gpu"},"action":"restore","immediate":true}"#,
    );

    // Options default when omitted.
    let req: BulkJobRequest =
        serde_json::from_str(r#"{"target":{"kind":"all"},"action":"pause"}"#).unwrap();
    assert_eq!(req.action, JobAction::Pause(PauseOptions::default()));
}

#[test]
fn every_action_round_trips() {
    let actions = [
        JobAction::Pause(PauseOptions { wait: true }),
        JobAction::Start,
        JobAction::Kill(KillOptions::default()),
        JobAction::Checkpoint,
        JobAction::Restore(RestoreOptions::default()),
        JobAction::Restart(RestartOptions {
            immediate: true,
            stashed: false,
            in_place: true,
        }),
        JobAction::Stash(StashOptions {
            enqueue_at: Some(5),
        }),
        JobAction::Enqueue(EnqueueOptions::default()),
        JobAction::Remove,
    ];
    for action in actions {
        let req = BulkJobRequest {
            target: JobSelector::Ids {
                ids: vec![JobId(1)],
            },
            action,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(&format!(r#""action":"{}""#, req.action.name())));
        let back: BulkJobRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        assert_eq!(
            JobActionKind::from_segment(req.action.name()),
            Some(JobActionKind::from(&req.action))
        );
    }
}

#[test]
fn bulk_response_golden() {
    let resp = BulkJobResponse {
        results: vec![
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
        ],
    };
    assert!(!resp.all_ok());
    assert_golden(
        &resp,
        r#"{"results":[{"id":1,"status":"ok"},{"id":2,"status":"ok","new_id":9},{"id":3,"status":"error","error":{"code":"invalid_state","message":"job 3 is not running"}}]}"#,
    );
}

#[test]
fn done_state_shape() {
    // Pins how puzuz-core's JobState nests inside API views.
    let state = JobState::Done {
        result: JobResult::Failed { exit_code: 1 },
    };
    assert_golden(
        &state,
        r#"{"state":"done","result":{"result":"failed","exit_code":1}}"#,
    );
}

#[test]
fn event_golden_and_sse() {
    let event = Event::JobRemoved { id: JobId(4) };
    assert_golden(&event, r#"{"type":"job_removed","id":4}"#);
    assert_eq!(
        event.to_sse(),
        "event: job_removed\ndata: {\"type\":\"job_removed\",\"id\":4}\n\n"
    );

    let event = Event::GroupUpdated {
        group: Group {
            name: "gpu".into(),
            parallel: 2,
            paused: false,
        },
    };
    assert_golden(
        &event,
        r#"{"type":"group_updated","group":{"name":"gpu","parallel":2,"paused":false}}"#,
    );
    assert_golden(&Event::DaemonShutdown, r#"{"type":"daemon_shutdown"}"#);

    let event = Event::JobUpdated {
        job: JobView::redacted(&sample_job()),
    };
    let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
    assert_eq!(back, event);
}

#[test]
fn resource_bodies_golden() {
    assert_golden(
        &SetGpusParallelRequest {
            gpus: None,
            parallel: 2,
        },
        r#"{"parallel":2}"#,
    );
    assert_golden(
        &CreateGroupRequest {
            name: "gpu".into(),
            parallel: Some(3),
        },
        r#"{"name":"gpu","parallel":3}"#,
    );
    assert_golden(
        &VersionResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            helper: HelperStatus {
                connected: true,
                version: Some("0.1.0".into()),
            },
        },
        r#"{"api_version":1,"daemon_version":"0.1.0","helper":{"connected":true,"version":"0.1.0"}}"#,
    );
    assert_golden(
        &GpuView {
            index: GpuIndex(0),
            uuid: "GPU-1".into(),
            name: "RTX".into(),
            memory_total_bytes: None,
            driver_version: None,
            available: true,
            parallel: 1,
            used: 1,
            jobs: vec![GpuJob {
                id: JobId(7),
                owner: Uid(1000),
            }],
        },
        r#"{"index":0,"uuid":"GPU-1","name":"RTX","parallel":1,"used":1,"jobs":[{"id":7,"owner":1000}]}"#,
    );
    // Empty bodies are accepted for option-only requests.
    let shutdown: ShutdownRequest = serde_json::from_str("{}").unwrap();
    assert!(!shutdown.no_checkpoint);
}

#[test]
fn routes() {
    let cases = [
        (Route::ListJobs, Method::Get, "/v1/jobs"),
        (Route::AddJob, Method::Post, "/v1/jobs"),
        (Route::GetJob(JobId(3)), Method::Get, "/v1/jobs/3"),
        (
            Route::JobAction(JobId(3), JobActionKind::Kill),
            Method::Post,
            "/v1/jobs/3/kill",
        ),
        (
            Route::JobAction(JobId(3), JobActionKind::Remove),
            Method::Delete,
            "/v1/jobs/3",
        ),
        (Route::BulkJobAction, Method::Post, "/v1/jobs/bulk"),
        (Route::JobLog(JobId(3)), Method::Get, "/v1/jobs/3/log"),
        (
            Route::SetGroupParallel("a b/c".into()),
            Method::Put,
            "/v1/groups/a%20b%2Fc/parallel",
        ),
        (
            Route::DeleteGroup("gpu".into()),
            Method::Delete,
            "/v1/groups/gpu",
        ),
        (
            Route::SetGpuParallel(GpuIndex(1)),
            Method::Put,
            "/v1/gpus/1/parallel",
        ),
        (Route::SetGpusParallel, Method::Put, routes::GPUS_PARALLEL),
        (Route::Shutdown, Method::Post, "/v1/daemon/shutdown"),
        (Route::Version, Method::Get, "/v1/version"),
        (Route::Events, Method::Get, "/v1/events"),
    ];
    for (route, method, path) in cases {
        assert_eq!(route.method(), method, "{route:?}");
        assert_eq!(route.path(), path, "{route:?}");
        assert!(route.path().starts_with(API_PREFIX));
    }
}

#[test]
fn query_strings() {
    assert_eq!(LogQuery::default().to_query_string(), "");
    assert_eq!(
        LogQuery {
            tail: Some(50),
            follow: true
        }
        .to_query_string(),
        "tail=50&follow=true"
    );
    assert_eq!(
        ListJobsQuery {
            group: Some("my group".into())
        }
        .to_query_string(),
        "group=my%20group"
    );
}
