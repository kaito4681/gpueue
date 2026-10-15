//! Golden and round-trip tests of the CLI ⇄ daemon wire format.
//!
//! The golden strings pin the exact JSON shape. If one of these fails, the
//! wire format changed: make sure that is intended and compatible.

use std::collections::BTreeMap;
use std::fmt::Debug;

use gpueue_core::model::{Caller, GpuIndex, Group, Job, JobId, JobResult, JobSpec, JobState, Uid};
use gpueue_protocol::api::routes::{self, JobActionKind};
use gpueue_protocol::api::*;
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
            nice: 0,
        },
        state: JobState::Running,
        created_at: 1_000,
        started_at: Some(2_000),
        finished_at: None,
        site_factor: 0,
        top_seq: None,
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
    let err = ApiError::from(gpueue_core::Error::JobNotFound(JobId(3)));
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
        assert_eq!(details.envs.as_ref(), Some(&job.spec.envs));
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
fn list_view_leaves_out_envs() {
    let job = sample_job();
    let view = JobView::full(&job).without_envs();
    assert_golden(
        &view,
        r#"{"id":7,"owner":1000,"group":"default","gpus":[0,1],"dependencies":[],"priority":0,"label":"run1","no_checkpoint":false,"state":{"state":"running"},"created_at":1000,"started_at":2000,"finished_at":null,"details":{"command":"python train.py --secret-token abc","cwd":"/home/alice/proj"}}"#,
    );
    assert!(!view.is_redacted());

    // An empty environment is still sent as such in the full view.
    let mut empty = sample_job();
    empty.spec.envs.clear();
    let json = serde_json::to_string(&JobView::full(&empty)).unwrap();
    assert!(json.ends_with(r#""envs":{}}}"#), "{json}");

    // Dropping envs from a redacted view changes nothing.
    let stranger = Caller {
        uid: Uid(1001),
        is_admin: false,
    };
    let redacted = JobView::for_caller(&job, &stranger);
    assert_eq!(redacted.clone().without_envs(), redacted);
}

#[test]
fn result_reasons_are_the_owners_last_error_only() {
    let stranger = Caller {
        uid: Uid(1001),
        is_admin: false,
    };
    for result in [
        JobResult::FailedToSpawn {
            reason: "no such directory /home/alice/proj".into(),
        },
        JobResult::CheckpointFailed {
            reason: "criu: /home/alice/proj busy".into(),
        },
    ] {
        let reason = match &result {
            JobResult::FailedToSpawn { reason } | JobResult::CheckpointFailed { reason } => {
                reason.clone()
            }
            _ => unreachable!(),
        };
        for state in [
            JobState::Done {
                result: result.clone(),
            },
            JobState::Terminating {
                result: result.clone(),
            },
        ] {
            let job = Job {
                state,
                ..sample_job()
            };
            let full = JobView::full(&job);
            assert_eq!(
                full.details.unwrap().last_error.as_deref(),
                Some(reason.as_str())
            );
            let json = serde_json::to_string(&JobView::for_caller(&job, &stranger)).unwrap();
            assert!(!json.contains("/home/alice"), "leaked: {json}");
            assert!(!json.contains("last_error"), "leaked: {json}");
        }
    }
    let killed = Job {
        state: JobState::Done {
            result: JobResult::Killed,
        },
        ..sample_job()
    };
    assert_eq!(JobView::full(&killed).details.unwrap().last_error, None);
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
            nice: 0,
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
                    launch_pending: false,
                    queued: None,
                },
            },
            JobActionResult::error(
                JobId(3),
                ApiError::new(ErrorCode::InvalidState, "job 3 is not running"),
            ),
            JobActionResult {
                id: JobId(4),
                outcome: ActionOutcome::Ok {
                    new_id: None,
                    launch_pending: true,
                    queued: None,
                },
            },
            JobActionResult {
                id: JobId(5),
                outcome: ActionOutcome::Ok {
                    new_id: None,
                    launch_pending: false,
                    queued: Some(QueuedLaunch::First),
                },
            },
        ],
        groups: Vec::new(),
    };
    assert!(!resp.all_ok());
    assert_golden(
        &resp,
        r#"{"results":[{"id":1,"status":"ok"},{"id":2,"status":"ok","new_id":9},{"id":3,"status":"error","error":{"code":"invalid_state","message":"job 3 is not running"}},{"id":4,"status":"ok","launch_pending":true},{"id":5,"status":"ok","queued":"first"}]}"#,
    );
    // Groups whose paused flag was set are listed; the field is optional.
    let paused = BulkJobResponse {
        results: Vec::new(),
        groups: vec!["default".into(), "gpu".into()],
    };
    assert_golden(&paused, r#"{"results":[],"groups":["default","gpu"]}"#);
    let plain: BulkJobResponse = serde_json::from_str(r#"{"results":[]}"#).unwrap();
    assert_eq!(plain, BulkJobResponse::new(Vec::new()));
    // The field is optional for older daemons.
    let add: AddJobResponse = serde_json::from_str(r#"{"id":5}"#).unwrap();
    assert_eq!(add, AddJobResponse::new(JobId(5)));
    assert_golden(&add, r#"{"id":5}"#);
    assert_golden(
        &AddJobResponse {
            id: JobId(6),
            launch_pending: true,
            queued: None,
        },
        r#"{"id":6,"launch_pending":true}"#,
    );
    assert_golden(
        &AddJobResponse {
            id: JobId(7),
            launch_pending: false,
            queued: Some(QueuedLaunch::InLine),
        },
        r#"{"id":7,"queued":"in_line"}"#,
    );
}

#[test]
fn done_state_shape() {
    // Pins how gpueue-core's JobState appears inside API views: the result's
    // fields are flattened next to `state`.
    let state = JobState::Done {
        result: JobResult::Failed { exit_code: 1 },
    };
    assert_golden(
        &state,
        r#"{"state":"done","result":"failed","exit_code":1}"#,
    );
    let state = JobState::Terminating {
        result: JobResult::CheckpointFailed {
            reason: "criu".into(),
        },
    };
    assert_golden(
        &state,
        r#"{"state":"terminating","result":"checkpoint_failed","reason":"criu"}"#,
    );
    let mut job = sample_job();
    job.state = JobState::Done {
        result: JobResult::Killed,
    };
    job.finished_at = Some(3_000);
    let stranger = Caller {
        uid: Uid(1001),
        is_admin: false,
    };
    assert_golden(
        &JobView::for_caller(&job, &stranger),
        r#"{"id":7,"owner":1000,"group":"default","gpus":[0,1],"dependencies":[],"priority":0,"label":null,"no_checkpoint":false,"state":{"state":"done","result":"killed"},"created_at":1000,"started_at":2000,"finished_at":3000}"#,
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
            priority: 0,
        },
    };
    assert_golden(
        &event,
        r#"{"type":"group_updated","group":{"name":"gpu","parallel":2,"paused":false}}"#,
    );
    assert_golden(&Event::DaemonShutdown, r#"{"type":"daemon_shutdown"}"#);
    assert_golden(&Event::Resync, r#"{"type":"resync"}"#);

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
                missing_programs: Vec::new(),
                programs: Vec::new(),
            },
            checkpoints: None,
            gpu_detection: None,
            schema_version: None,
            orphan_jobs: None,
        },
        r#"{"api_version":1,"daemon_version":"0.1.0","helper":{"connected":true,"version":"0.1.0"}}"#,
    );
    let with_usage = VersionResponse {
        api_version: API_VERSION,
        daemon_version: "0.1.0".into(),
        helper: HelperStatus {
            connected: false,
            version: None,
            missing_programs: Vec::new(),
            programs: Vec::new(),
        },
        checkpoints: Some(CheckpointStorage {
            images: 2,
            total_bytes: 1500,
            limit_bytes: Some(1000),
            available_bytes: Some(1 << 40),
        }),
        gpu_detection: None,
        schema_version: None,
        orphan_jobs: None,
    };
    assert_golden(
        &with_usage,
        concat!(
            r#"{"api_version":1,"daemon_version":"0.1.0","helper":{"connected":false},"#,
            r#""checkpoints":{"images":2,"total_bytes":1500,"limit_bytes":1000,"#,
            r#""available_bytes":1099511627776}}"#
        ),
    );
    assert!(with_usage.checkpoints.as_ref().unwrap().over_limit());
    // Without a limit or free space (e.g. the directory is missing).
    assert_golden(
        &CheckpointStorage {
            images: 0,
            total_bytes: 0,
            limit_bytes: None,
            available_bytes: None,
        },
        r#"{"images":0,"total_bytes":0}"#,
    );
    let unlimited: CheckpointStorage =
        serde_json::from_str(r#"{"images":1,"total_bytes":5}"#).unwrap();
    assert!(!unlimited.over_limit());
    assert_golden(
        &GpuDetectionStatus {
            mode: GpuDetection::Auto,
            source: GpuSource::Config,
            nvml_error: Some("NVIDIA driver is not loaded".into()),
            gpus: vec![DetectedGpuSummary {
                index: GpuIndex(0),
                uuid: "GPU-a".into(),
                name: "A100".into(),
            }],
        },
        concat!(
            r#"{"mode":"auto","source":"config","nvml_error":"NVIDIA driver is not loaded","#,
            r#""gpus":[{"index":0,"uuid":"GPU-a","name":"A100"}]}"#
        ),
    );
    assert_golden(
        &GpuDetectionStatus {
            mode: GpuDetection::Static,
            source: GpuSource::Nvml,
            nvml_error: None,
            gpus: vec![],
        },
        r#"{"mode":"static","source":"nvml","gpus":[]}"#,
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
fn version_response_from_a_newer_daemon_decodes() {
    // A new GPU source decodes as `Unknown`.
    let version: VersionResponse = serde_json::from_str(concat!(
        r#"{"api_version":1,"daemon_version":"9.0.0","helper":{"connected":true},"#,
        r#""gpu_detection":{"mode":"auto","source":"pci_scan","gpus":[]}}"#
    ))
    .unwrap();
    assert_eq!(version.gpu_detection.unwrap().source, GpuSource::Unknown);
    // A new mode (or any shape this client does not know) drops the
    // diagnostic field instead of failing the whole response.
    let version: VersionResponse = serde_json::from_str(concat!(
        r#"{"api_version":2,"daemon_version":"9.0.0","helper":{"connected":false},"#,
        r#""checkpoints":{"images":"many"},"#,
        r#""gpu_detection":{"mode":"dynamic","source":"nvml","gpus":[]},"new_field":true}"#
    ))
    .unwrap();
    assert_eq!(version.api_version, 2);
    assert_eq!(version.daemon_version, "9.0.0");
    assert_eq!(version.checkpoints, None);
    assert_eq!(version.gpu_detection, None);
    // Explicit nulls are still accepted.
    let version: VersionResponse = serde_json::from_str(
        r#"{"api_version":1,"daemon_version":"x","helper":{"connected":true},"gpu_detection":null}"#,
    )
    .unwrap();
    assert_eq!(version.gpu_detection, None);
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
        (Route::NiceJobs, Method::Post, "/v1/jobs/nice"),
        (Route::TopJobs, Method::Post, "/v1/jobs/top"),
        (Route::SiteJobs, Method::Post, "/v1/jobs/site"),
        (Route::Priority, Method::Get, "/v1/priority"),
        (Route::ListShares, Method::Get, "/v1/shares"),
        (
            Route::SetShares(gpueue_core::model::Uid(1000)),
            Method::Put,
            "/v1/shares/1000",
        ),
        (
            Route::SetGroupPriority("a b".into()),
            Method::Put,
            "/v1/groups/a%20b/priority",
        ),
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
            group: Some("my group".into()),
            details: None,
        }
        .to_query_string(),
        "group=my%20group"
    );
    assert_eq!(
        ListJobsQuery {
            group: Some("gpu".into()),
            details: Some(ListDetails::Envs),
        }
        .to_query_string(),
        "group=gpu&details=envs"
    );
    assert!(!ListJobsQuery::default().include_envs());
    assert_eq!(ListJobsQuery::default().to_query_string(), "");
}
