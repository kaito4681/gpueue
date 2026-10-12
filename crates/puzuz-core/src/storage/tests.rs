use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use rusqlite::Connection;

use super::*;
use crate::error::Error;
use crate::model::{DEFAULT_GROUP, JobResult};

fn spec(command: &str) -> JobSpec {
    JobSpec {
        command: command.to_owned(),
        cwd: PathBuf::from("/home/alice/project"),
        envs: BTreeMap::new(),
        group: DEFAULT_GROUP.to_owned(),
        gpus: Vec::new(),
        dependencies: Vec::new(),
        priority: 0,
        label: None,
        no_checkpoint: false,
    }
}

fn full_spec() -> JobSpec {
    JobSpec {
        command: "python train.py --epochs 10".to_owned(),
        // Not valid UTF-8: paths are stored as raw bytes.
        cwd: PathBuf::from(OsStr::from_bytes(b"/home/alice/\xffproj")),
        envs: BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("CUDA_DEVICE_ORDER".to_owned(), "PCI_BUS_ID".to_owned()),
            ("EMPTY".to_owned(), String::new()),
        ]),
        group: DEFAULT_GROUP.to_owned(),
        gpus: vec![GpuIndex(3), GpuIndex(0), GpuIndex(1)],
        dependencies: vec![JobId(7), JobId(2)],
        priority: -5,
        label: Some("exp-42".to_owned()),
        no_checkpoint: true,
    }
}

fn queued(at: Timestamp) -> JobState {
    JobState::Queued { enqueued_at: at }
}

fn assert_domain(err: StorageError, expected: Error) {
    match err {
        StorageError::Domain(e) => assert_eq!(e, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .conn
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

// ----- opening and migrations -----

#[test]
fn in_memory_store_is_migrated() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, migrations::MIGRATIONS.len() as i64);
    let fk: bool = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert!(fk);
}

#[test]
fn on_disk_store_uses_wal_normal_sync_and_private_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let store = Store::open(&path).unwrap();

    let mode: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    // NORMAL == 1
    let sync: i64 = store
        .conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1);
    let fk: bool = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert!(fk);

    // Force the WAL file into existence and check every file is private.
    store.create_group("gpu", 2).unwrap();
    for name in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let meta = std::fs::metadata(dir.path().join(name)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{name}");
    }
}

#[test]
fn open_does_not_change_mode_of_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(Store::open(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    drop(Store::open(&path).unwrap());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
}

#[test]
fn open_fails_cleanly_for_missing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let err = Store::open(dir.path().join("missing/state.sqlite")).unwrap_err();
    assert!(matches!(err, StorageError::Io { .. }), "{err:?}");
}

#[test]
fn reopening_is_idempotent_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let store = Store::open(&path).unwrap();
        store.create_group("gpu", 4).unwrap();
        store
            .insert_job(Uid(1000), &full_spec(), &queued(5), 5)
            .unwrap();
    }
    for _ in 0..3 {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The seeded default group is not duplicated or reset.
        assert_eq!(store.list_groups().unwrap().len(), 2);
        assert_eq!(store.get_group("gpu").unwrap().unwrap().parallel, 4);
        let job = store.get_job(JobId(0)).unwrap().unwrap();
        assert_eq!(job.spec, full_spec());
        assert_eq!(store.next_job_id().unwrap(), JobId(1));
    }
}

#[test]
fn future_schema_version_is_rejected_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(Store::open(&path).unwrap());
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
    }
    let err = Store::open(&path).unwrap_err();
    match &err {
        StorageError::UnsupportedSchemaVersion { found, supported } => {
            assert_eq!(*found, SCHEMA_VERSION + 1);
            assert_eq!(*supported, SCHEMA_VERSION);
        }
        other => panic!("unexpected error {other:?}"),
    }
    let message = err.to_string();
    assert!(message.contains("newer"), "{message}");
    assert!(message.contains("upgrade puzuz"), "{message}");

    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION + 1);
}

#[test]
fn failed_migration_leaves_database_at_old_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        // A table clashing with the first migration makes it fail midway.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE gpus (x INTEGER)").unwrap();
    }
    assert!(Store::open(&path).is_err());
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 0);
    let meta_tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('meta', 'groups', 'jobs')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(meta_tables, 0, "partial migration was not rolled back");
}

// ----- jobs -----

#[test]
fn insert_and_get_job_round_trips_every_field() {
    let store = Store::open_in_memory().unwrap();
    let job = store
        .insert_job(Uid(1000), &full_spec(), &queued(10), 9)
        .unwrap();
    assert_eq!(job.id, JobId(0));
    assert_eq!(job.owner, Uid(1000));
    assert_eq!(job.created_at, 9);
    assert_eq!(job.started_at, None);
    assert_eq!(job.finished_at, None);

    let loaded = store.get_job(job.id).unwrap().unwrap();
    assert_eq!(loaded, job);
    // GPU and dependency order is preserved, not sorted.
    assert_eq!(
        loaded.spec.gpus,
        vec![GpuIndex(3), GpuIndex(0), GpuIndex(1)]
    );
    assert_eq!(loaded.spec.dependencies, vec![JobId(7), JobId(2)]);
}

#[test]
fn every_job_state_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let states = [
        queued(1),
        JobState::Stashed { enqueue_at: None },
        JobState::Stashed {
            enqueue_at: Some(123),
        },
        JobState::Running,
        JobState::Paused,
        JobState::Checkpointing,
        JobState::Checkpointed,
        JobState::RestoreQueued { enqueued_at: 2 },
        JobState::Restoring,
        JobState::Done {
            result: JobResult::Success,
        },
        JobState::Done {
            result: JobResult::Failed { exit_code: -3 },
        },
        JobState::Done {
            result: JobResult::FailedToSpawn {
                reason: "no such file".to_owned(),
            },
        },
        JobState::Done {
            result: JobResult::Killed,
        },
        JobState::Done {
            result: JobResult::DependencyFailed,
        },
        JobState::Done {
            result: JobResult::CheckpointFailed {
                reason: "criu: dump failed".to_owned(),
            },
        },
        JobState::Done {
            result: JobResult::Lost,
        },
    ];
    for state in states {
        let job = store.insert_job(Uid(1), &spec("true"), &state, 0).unwrap();
        assert_eq!(store.get_job(job.id).unwrap().unwrap().state, state);
        let stored_name: String = store
            .conn
            .query_row("SELECT state FROM jobs WHERE id = ?1", [job.id.0], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored_name, state.name());
    }
}

#[test]
fn get_missing_job_returns_none() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.get_job(JobId(42)).unwrap(), None);
}

#[test]
fn insert_into_unknown_group_fails_without_consuming_an_id() {
    let store = Store::open_in_memory().unwrap();
    let mut s = spec("true");
    s.group = "nope".to_owned();
    let err = store.insert_job(Uid(1), &s, &queued(0), 0).unwrap_err();
    assert_domain(err, Error::GroupNotFound("nope".to_owned()));
    assert_eq!(store.next_job_id().unwrap(), JobId(0));
    assert_eq!(count(&store, "jobs"), 0);
}

#[test]
fn failed_insert_rolls_back_partial_rows() {
    let store = Store::open_in_memory().unwrap();
    let mut s = spec("true");
    // Duplicate GPU violates UNIQUE (job_id, gpu_index) after the job row,
    // the env rows and the counter bump were already written.
    s.envs.insert("A".to_owned(), "1".to_owned());
    s.gpus = vec![GpuIndex(0), GpuIndex(0)];
    assert!(store.insert_job(Uid(1), &s, &queued(0), 0).is_err());
    assert_eq!(count(&store, "jobs"), 0);
    assert_eq!(count(&store, "job_env"), 0);
    assert_eq!(count(&store, "job_gpus"), 0);
    assert_eq!(store.next_job_id().unwrap(), JobId(0));
    // The store is still usable (the savepoint was released).
    let job = store
        .insert_job(Uid(1), &spec("true"), &queued(0), 0)
        .unwrap();
    assert_eq!(job.id, JobId(0));
}

#[test]
fn job_ids_increase_and_are_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let store = Store::open(&path).unwrap();
        for expected in 0..3 {
            let job = store
                .insert_job(Uid(1), &spec("true"), &queued(0), 0)
                .unwrap();
            assert_eq!(job.id, JobId(expected));
        }
        // Removing the highest id must not let it be handed out again.
        store.remove_job(JobId(2)).unwrap();
        store.remove_job(JobId(1)).unwrap();
        assert_eq!(store.next_job_id().unwrap(), JobId(3));
    }
    let store = Store::open(&path).unwrap();
    let job = store
        .insert_job(Uid(1), &spec("true"), &queued(0), 0)
        .unwrap();
    assert_eq!(job.id, JobId(3));
}

#[test]
fn list_jobs_filters_by_owner_group_and_state() {
    let store = Store::open_in_memory().unwrap();
    store.create_group("gpu", 1).unwrap();
    let mut gpu_spec = spec("train");
    gpu_spec.group = "gpu".to_owned();

    let a = store.insert_job(Uid(1), &spec("a"), &queued(0), 0).unwrap();
    let b = store
        .insert_job(Uid(2), &gpu_spec, &JobState::Running, 0)
        .unwrap();
    let c = store.insert_job(Uid(1), &gpu_spec, &queued(0), 0).unwrap();
    let d = store
        .insert_job(
            Uid(2),
            &spec("d"),
            &JobState::Stashed { enqueue_at: None },
            0,
        )
        .unwrap();

    let ids = |filter: JobFilter<'_>| -> Vec<JobId> {
        store
            .list_jobs(&filter)
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect()
    };
    assert_eq!(ids(JobFilter::all()), vec![a.id, b.id, c.id, d.id]);
    assert_eq!(ids(JobFilter::all().owner(Uid(1))), vec![a.id, c.id]);
    assert_eq!(ids(JobFilter::all().group("gpu")), vec![b.id, c.id]);
    assert_eq!(ids(JobFilter::all().state("queued")), vec![a.id, c.id]);
    assert_eq!(
        ids(JobFilter::all().owner(Uid(1)).group("gpu").state("queued")),
        vec![c.id]
    );
    assert_eq!(ids(JobFilter::all().owner(Uid(3))), Vec::<JobId>::new());
    assert_eq!(ids(JobFilter::all().state("done")), Vec::<JobId>::new());

    // Listed jobs are complete, including child tables.
    let full = store
        .insert_job(Uid(9), &full_spec(), &queued(0), 0)
        .unwrap();
    let listed = store.list_jobs(&JobFilter::all().owner(Uid(9))).unwrap();
    assert_eq!(listed, vec![full]);
}

#[test]
fn update_job_state_sets_state_and_timestamps() {
    let store = Store::open_in_memory().unwrap();
    let job = store
        .insert_job(Uid(1), &spec("true"), &queued(0), 0)
        .unwrap();

    store
        .update_job_state(job.id, &JobState::Running, Some(100), None)
        .unwrap();
    let running = store.get_job(job.id).unwrap().unwrap();
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.started_at, Some(100));
    assert_eq!(running.finished_at, None);
    assert_eq!(
        store
            .list_jobs(&JobFilter::all().state("running"))
            .unwrap()
            .len(),
        1
    );

    let done = JobState::Done {
        result: JobResult::Failed { exit_code: 2 },
    };
    store
        .update_job_state(job.id, &done, Some(100), Some(200))
        .unwrap();
    let finished = store.get_job(job.id).unwrap().unwrap();
    assert_eq!(finished.state, done);
    assert_eq!(finished.started_at, Some(100));
    assert_eq!(finished.finished_at, Some(200));
    // The spec is untouched.
    assert_eq!(finished.spec, spec("true"));

    let err = store
        .update_job_state(JobId(99), &JobState::Running, None, None)
        .unwrap_err();
    assert_domain(err, Error::JobNotFound(JobId(99)));
}

#[test]
fn remove_job_cascades_to_child_tables() {
    let store = Store::open_in_memory().unwrap();
    let keep = store
        .insert_job(Uid(1), &full_spec(), &queued(0), 0)
        .unwrap();
    let gone = store
        .insert_job(Uid(1), &full_spec(), &JobState::Checkpointed, 0)
        .unwrap();
    store.put_checkpoint(&checkpoint(gone.id)).unwrap();

    store.remove_job(gone.id).unwrap();
    assert_eq!(store.get_job(gone.id).unwrap(), None);
    assert_eq!(store.get_checkpoint(gone.id).unwrap(), None);
    for table in ["job_env", "job_gpus", "job_deps"] {
        let n: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE job_id = ?1"),
                [gone.id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    assert_eq!(store.get_job(keep.id).unwrap().unwrap(), keep);

    assert_domain(
        store.remove_job(gone.id).unwrap_err(),
        Error::JobNotFound(gone.id),
    );
}

#[test]
fn dependency_on_removed_job_is_kept() {
    let store = Store::open_in_memory().unwrap();
    let parent = store.insert_job(Uid(1), &spec("a"), &queued(0), 0).unwrap();
    let mut child_spec = spec("b");
    child_spec.dependencies = vec![parent.id];
    let child = store
        .insert_job(Uid(1), &child_spec, &queued(0), 0)
        .unwrap();
    store.remove_job(parent.id).unwrap();
    assert_eq!(
        store.get_job(child.id).unwrap().unwrap().spec.dependencies,
        vec![parent.id]
    );
}

#[test]
fn repeated_dependencies_are_recorded_once_in_order() {
    let store = Store::open_in_memory().unwrap();
    let mut child_spec = spec("c");
    child_spec.dependencies = vec![JobId(5), JobId(2), JobId(5), JobId(2), JobId(9)];
    let child = store
        .insert_job(Uid(1), &child_spec, &queued(0), 0)
        .unwrap();
    let expected = vec![JobId(5), JobId(2), JobId(9)];
    assert_eq!(child.spec.dependencies, expected);
    assert_eq!(
        store.get_job(child.id).unwrap().unwrap().spec.dependencies,
        expected
    );
}

// ----- transactions -----

#[test]
fn transaction_commits_on_ok() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .transaction(|tx| {
            let job = tx.insert_job(Uid(1), &spec("a"), &queued(0), 0)?;
            tx.update_job_state(job.id, &JobState::Running, Some(1), None)?;
            tx.set_group_parallel(DEFAULT_GROUP, 3)?;
            Ok::<_, StorageError>(job.id)
        })
        .unwrap();
    assert_eq!(store.get_job(id).unwrap().unwrap().state, JobState::Running);
    assert_eq!(store.get_group(DEFAULT_GROUP).unwrap().unwrap().parallel, 3);
}

#[test]
fn transaction_rolls_back_on_error() {
    let mut store = Store::open_in_memory().unwrap();
    let err = store
        .transaction(|tx| {
            tx.insert_job(Uid(1), &spec("a"), &queued(0), 0)?;
            tx.create_group("gpu", 1)?;
            tx.update_job_state(JobId(99), &JobState::Running, None, None)
        })
        .unwrap_err();
    assert_domain(err, Error::JobNotFound(JobId(99)));
    assert_eq!(count(&store, "jobs"), 0);
    assert_eq!(store.get_group("gpu").unwrap(), None);
    // The id counter is rolled back too, so no id is skipped.
    assert_eq!(store.next_job_id().unwrap(), JobId(0));
}

#[test]
fn transaction_rolls_back_on_panic() {
    let mut store = Store::open_in_memory().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<()> = store.transaction(|tx| {
            tx.create_group("gpu", 1)?;
            panic!("boom");
        });
    }));
    assert!(result.is_err());
    assert_eq!(store.get_group("gpu").unwrap(), None);
    // The connection is usable afterwards.
    store.create_group("gpu", 1).unwrap();
}

#[test]
fn transaction_accepts_caller_error_types() {
    #[derive(Debug)]
    enum AppError {
        Storage(#[allow(dead_code)] StorageError),
        Rejected,
    }
    impl From<StorageError> for AppError {
        fn from(e: StorageError) -> Self {
            AppError::Storage(e)
        }
    }
    let mut store = Store::open_in_memory().unwrap();
    let err = store
        .transaction(|tx| {
            tx.create_group("gpu", 1)?;
            Err::<(), _>(AppError::Rejected)
        })
        .unwrap_err();
    assert!(matches!(err, AppError::Rejected));
    assert_eq!(store.get_group("gpu").unwrap(), None);
}

#[test]
fn multi_statement_op_inside_failed_transaction_is_rolled_back() {
    let mut store = Store::open_in_memory().unwrap();
    let _ = store.transaction(|tx| {
        // A failing savepointed op inside a transaction must not end the
        // outer transaction, and the outer rollback still undoes earlier ops.
        tx.create_group("gpu", 1)?;
        let mut bad = spec("a");
        bad.group = "nope".to_owned();
        assert!(tx.insert_job(Uid(1), &bad, &queued(0), 0).is_err());
        assert!(tx.get_group("gpu")?.is_some());
        Err::<(), _>(StorageError::Corrupt("abort".to_owned()))
    });
    assert_eq!(store.get_group("gpu").unwrap(), None);
}

// ----- groups -----

#[test]
fn default_group_exists_after_migration() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(
        store.list_groups().unwrap(),
        vec![Group {
            name: DEFAULT_GROUP.to_owned(),
            parallel: 1,
            paused: false,
        }]
    );
}

#[test]
fn group_create_configure_and_list() {
    let store = Store::open_in_memory().unwrap();
    let g = store.create_group("gpu", 2).unwrap();
    assert_eq!(
        g,
        Group {
            name: "gpu".to_owned(),
            parallel: 2,
            paused: false
        }
    );
    assert!(matches!(
        store.create_group("gpu", 5).unwrap_err(),
        StorageError::GroupExists(name) if name == "gpu"
    ));
    assert_eq!(store.get_group("gpu").unwrap().unwrap().parallel, 2);

    store.set_group_parallel("gpu", 8).unwrap();
    store.set_group_paused("gpu", true).unwrap();
    assert_eq!(
        store.get_group("gpu").unwrap().unwrap(),
        Group {
            name: "gpu".to_owned(),
            parallel: 8,
            paused: true
        }
    );
    store.set_group_paused("gpu", false).unwrap();
    assert!(!store.get_group("gpu").unwrap().unwrap().paused);

    let names: Vec<_> = store
        .list_groups()
        .unwrap()
        .into_iter()
        .map(|g| g.name)
        .collect();
    assert_eq!(names, vec!["default", "gpu"]);

    assert_domain(
        store.set_group_parallel("nope", 1).unwrap_err(),
        Error::GroupNotFound("nope".to_owned()),
    );
    assert_domain(
        store.set_group_paused("nope", true).unwrap_err(),
        Error::GroupNotFound("nope".to_owned()),
    );
    assert_eq!(store.get_group("nope").unwrap(), None);
}

#[test]
fn group_removal_rules() {
    let store = Store::open_in_memory().unwrap();
    assert!(matches!(
        store.remove_group(DEFAULT_GROUP).unwrap_err(),
        StorageError::DefaultGroupRemoval
    ));
    assert_domain(
        store.remove_group("nope").unwrap_err(),
        Error::GroupNotFound("nope".to_owned()),
    );

    store.create_group("gpu", 1).unwrap();
    let mut s = spec("a");
    s.group = "gpu".to_owned();
    let job = store.insert_job(Uid(1), &s, &queued(0), 0).unwrap();
    assert!(matches!(
        store.remove_group("gpu").unwrap_err(),
        StorageError::GroupNotEmpty(name) if name == "gpu"
    ));

    store.remove_job(job.id).unwrap();
    store.remove_group("gpu").unwrap();
    assert_eq!(store.get_group("gpu").unwrap(), None);
}

// ----- GPUs -----

#[test]
fn gpu_upsert_keeps_configured_parallel() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.list_gpus().unwrap().is_empty());

    let g0 = store
        .upsert_gpu(GpuIndex(0), "GPU-aaa", "RTX 4090")
        .unwrap();
    assert_eq!(g0.parallel, 1);
    store
        .upsert_gpu(GpuIndex(1), "GPU-bbb", "RTX 4090")
        .unwrap();
    store.set_gpu_parallel(GpuIndex(0), 3).unwrap();

    // Re-detection (e.g. daemon restart, driver update renaming the model).
    let g0 = store
        .upsert_gpu(GpuIndex(0), "GPU-aaa", "NVIDIA GeForce RTX 4090")
        .unwrap();
    assert_eq!(
        g0,
        Gpu {
            index: GpuIndex(0),
            uuid: "GPU-aaa".to_owned(),
            name: "NVIDIA GeForce RTX 4090".to_owned(),
            parallel: 3,
        }
    );
    assert_eq!(store.get_gpu(GpuIndex(0)).unwrap().unwrap(), g0);
    let indices: Vec<_> = store
        .list_gpus()
        .unwrap()
        .into_iter()
        .map(|g| (g.index, g.parallel))
        .collect();
    assert_eq!(indices, vec![(GpuIndex(0), 3), (GpuIndex(1), 1)]);

    assert_eq!(store.get_gpu(GpuIndex(7)).unwrap(), None);
    assert_domain(
        store.set_gpu_parallel(GpuIndex(7), 2).unwrap_err(),
        Error::GpuNotFound(GpuIndex(7)),
    );
}

// ----- checkpoints -----

fn checkpoint(job_id: JobId) -> CheckpointRecord {
    CheckpointRecord {
        job_id,
        path: PathBuf::from(format!("/var/lib/puzuz/checkpoints/{job_id}")),
        size_bytes: None,
        gpu_model: Some("NVIDIA GeForce RTX 4090".to_owned()),
        driver_version: Some("570.86.10".to_owned()),
        created_at: 1_700_000_000_000,
        status: CheckpointStatus::InProgress,
        failure_reason: None,
        paused: false,
    }
}

#[test]
fn checkpoint_record_lifecycle() {
    let store = Store::open_in_memory().unwrap();
    let a = store
        .insert_job(Uid(1), &spec("a"), &JobState::Checkpointing, 0)
        .unwrap();
    let b = store
        .insert_job(Uid(1), &spec("b"), &JobState::Checkpointing, 0)
        .unwrap();

    assert_eq!(store.get_checkpoint(a.id).unwrap(), None);
    let rec_a = checkpoint(a.id);
    store.put_checkpoint(&rec_a).unwrap();
    assert_eq!(store.get_checkpoint(a.id).unwrap().unwrap(), rec_a);

    // Large sizes (> u32) round-trip.
    store
        .set_checkpoint_status(a.id, CheckpointStatus::Complete, Some(17_000_000_000), None)
        .unwrap();
    let done = store.get_checkpoint(a.id).unwrap().unwrap();
    assert_eq!(done.status, CheckpointStatus::Complete);
    assert_eq!(done.size_bytes, Some(17_000_000_000));

    let mut rec_b = checkpoint(b.id);
    rec_b.gpu_model = None;
    rec_b.driver_version = None;
    rec_b.paused = true;
    store.put_checkpoint(&rec_b).unwrap();
    assert_eq!(store.get_checkpoint(b.id).unwrap().unwrap(), rec_b);
    store
        .set_checkpoint_status(b.id, CheckpointStatus::Failed, None, Some("criu: ENOSPC"))
        .unwrap();
    // A status change keeps the paused flag.
    assert!(store.get_checkpoint(b.id).unwrap().unwrap().paused);

    let list = store.list_checkpoints().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].job_id, a.id);
    assert_eq!(list[1].job_id, b.id);
    assert_eq!(list[1].status, CheckpointStatus::Failed);
    assert_eq!(list[1].failure_reason.as_deref(), Some("criu: ENOSPC"));

    // Putting again replaces the single generation.
    let mut newer = checkpoint(a.id);
    newer.created_at += 1;
    store.put_checkpoint(&newer).unwrap();
    assert_eq!(store.get_checkpoint(a.id).unwrap().unwrap(), newer);
    assert_eq!(store.list_checkpoints().unwrap().len(), 2);

    assert!(store.delete_checkpoint(a.id).unwrap());
    assert!(!store.delete_checkpoint(a.id).unwrap());
    assert_eq!(store.get_checkpoint(a.id).unwrap(), None);
    assert!(matches!(
        store
            .set_checkpoint_status(a.id, CheckpointStatus::Complete, None, None)
            .unwrap_err(),
        StorageError::CheckpointNotFound(id) if id == a.id
    ));
}

#[test]
fn checkpoint_failures_are_reported_once_per_failure() {
    let store = Store::open_in_memory().unwrap();
    let a = store
        .insert_job(Uid(1), &spec("a"), &JobState::Checkpointing, 0)
        .unwrap();
    let b = store
        .insert_job(Uid(1), &spec("b"), &JobState::Checkpointing, 0)
        .unwrap();
    let other = store
        .insert_job(Uid(2), &spec("c"), &JobState::Checkpointing, 0)
        .unwrap();
    for id in [a.id, b.id, other.id] {
        store.put_checkpoint(&checkpoint(id)).unwrap();
    }
    // In-progress and complete dumps are not warnings.
    store
        .set_checkpoint_status(b.id, CheckpointStatus::Complete, Some(1), None)
        .unwrap();
    assert!(
        store
            .unacknowledged_checkpoint_failures(Uid(1))
            .unwrap()
            .is_empty()
    );

    for id in [a.id, other.id] {
        store
            .set_checkpoint_status(id, CheckpointStatus::Failed, None, Some("boom"))
            .unwrap();
    }
    assert_eq!(
        store.unacknowledged_checkpoint_failures(Uid(1)).unwrap(),
        [a.id]
    );
    assert_eq!(
        store.unacknowledged_checkpoint_failures(Uid(2)).unwrap(),
        [other.id]
    );

    // Acknowledging ignores records that did not fail.
    store
        .acknowledge_checkpoint_failures(&[a.id, b.id, JobId(99)])
        .unwrap();
    assert!(
        store
            .unacknowledged_checkpoint_failures(Uid(1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.get_checkpoint(b.id).unwrap().unwrap().status,
        CheckpointStatus::Complete
    );

    // Recording a restore error keeps the status and the acknowledgement.
    store
        .set_checkpoint_status(a.id, CheckpointStatus::Failed, None, Some("again"))
        .unwrap();
    assert!(
        store
            .unacknowledged_checkpoint_failures(Uid(1))
            .unwrap()
            .is_empty()
    );

    // A new dump that fails is a new warning.
    store.put_checkpoint(&checkpoint(a.id)).unwrap();
    store
        .set_checkpoint_status(a.id, CheckpointStatus::Failed, None, Some("boom"))
        .unwrap();
    assert_eq!(
        store.unacknowledged_checkpoint_failures(Uid(1)).unwrap(),
        [a.id]
    );
}

#[test]
fn version_one_database_is_upgraded_keeping_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(migrations::MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO jobs (id, owner_uid, command, cwd, group_name, state, state_data,
                               created_at)
             VALUES (0, 1, 'x', x'2f', 'default', 'checkpointing', ?1, 0)",
            [serde_json::to_string(&JobState::Checkpointing).unwrap()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO checkpoints (job_id, path, created_at, status, failure_reason)
             VALUES (0, x'2f', 0, 'failed', 'boom');",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let record = store.get_checkpoint(JobId(0)).unwrap().unwrap();
    assert_eq!(record.failure_reason.as_deref(), Some("boom"));
    assert!(!record.paused, "old records were not paused");
    assert_eq!(
        store.unacknowledged_checkpoint_failures(Uid(1)).unwrap(),
        [JobId(0)]
    );
}

#[test]
fn checkpoint_for_unknown_job_is_rejected() {
    let store = Store::open_in_memory().unwrap();
    assert_domain(
        store.put_checkpoint(&checkpoint(JobId(5))).unwrap_err(),
        Error::JobNotFound(JobId(5)),
    );
}

#[test]
fn checkpoint_size_beyond_i64_is_rejected() {
    let store = Store::open_in_memory().unwrap();
    let job = store.insert_job(Uid(1), &spec("a"), &queued(0), 0).unwrap();
    let mut rec = checkpoint(job.id);
    rec.size_bytes = Some(u64::MAX);
    assert!(store.put_checkpoint(&rec).is_err());
}

#[test]
fn corrupt_state_data_is_reported() {
    let store = Store::open_in_memory().unwrap();
    let job = store.insert_job(Uid(1), &spec("a"), &queued(0), 0).unwrap();
    store
        .conn
        .execute(
            "UPDATE jobs SET state = 'running' WHERE id = ?1",
            [job.id.0],
        )
        .unwrap();
    assert!(matches!(
        store.get_job(job.id).unwrap_err(),
        StorageError::Corrupt(_)
    ));
    store
        .conn
        .execute(
            "UPDATE jobs SET state_data = '{\"state\":\"bogus\"}' WHERE id = ?1",
            [job.id.0],
        )
        .unwrap();
    assert!(matches!(
        store.list_jobs(&JobFilter::all()).unwrap_err(),
        StorageError::Corrupt(_)
    ));
}

// ----- switch -----

#[test]
fn swap_job_ids_moves_every_row_and_dependency() {
    let mut store = Store::open_in_memory().unwrap();
    let a = store
        .insert_job(Uid(1000), &full_spec(), &queued(1), 1)
        .unwrap();
    let b = store
        .insert_job(Uid(1001), &spec("b"), &queued(2), 2)
        .unwrap();
    let mut dependant_spec = spec("c");
    dependant_spec.dependencies = vec![a.id];
    let c = store
        .insert_job(Uid(1000), &dependant_spec, &queued(3), 3)
        .unwrap();
    store
        .put_checkpoint(&CheckpointRecord {
            job_id: a.id,
            path: PathBuf::from("/ckpt/a"),
            size_bytes: None,
            gpu_model: None,
            driver_version: None,
            created_at: 5,
            status: CheckpointStatus::Complete,
            failure_reason: None,
            paused: false,
        })
        .unwrap();

    store.transaction(|tx| tx.swap_job_ids(a.id, b.id)).unwrap();

    let new_a = store.get_job(b.id).unwrap().unwrap();
    assert_eq!(new_a.owner, a.owner);
    assert_eq!(new_a.spec, a.spec);
    assert_eq!(new_a.created_at, a.created_at);
    let new_b = store.get_job(a.id).unwrap().unwrap();
    assert_eq!(new_b.spec, b.spec);
    assert_eq!(
        store.get_job(c.id).unwrap().unwrap().spec.dependencies,
        vec![b.id]
    );
    assert_eq!(
        store.get_checkpoint(b.id).unwrap().unwrap().path,
        PathBuf::from("/ckpt/a")
    );
    assert!(store.get_checkpoint(a.id).unwrap().is_none());
    // The counter was only borrowed.
    assert_eq!(store.next_job_id().unwrap(), JobId(3));

    // Outside an explicit transaction, too.
    store.swap_job_ids(a.id, b.id).unwrap();
    assert_eq!(store.get_job(a.id).unwrap().unwrap().spec, a.spec);

    assert_domain(
        store.swap_job_ids(a.id, JobId(99)).unwrap_err(),
        Error::JobNotFound(JobId(99)),
    );
}

#[test]
fn foreign_keys_are_checked_immediately_again_after_a_swap() {
    let mut store = Store::open_in_memory().unwrap();
    let a = store
        .insert_job(Uid(1000), &spec("a"), &queued(1), 1)
        .unwrap();
    let b = store
        .insert_job(Uid(1000), &spec("b"), &queued(2), 2)
        .unwrap();
    store
        .transaction(|tx| {
            tx.swap_job_ids(a.id, b.id)?;
            let deferred: bool = tx
                .tx
                .query_row("PRAGMA defer_foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert!(!deferred);
            // A dangling reference fails right away rather than at commit.
            assert!(tx.put_checkpoint(&checkpoint(JobId(99))).is_err());
            Ok::<_, StorageError>(())
        })
        .unwrap();
}

// ----- helper events -----

#[test]
fn helper_event_seq_starts_at_zero_and_is_kept() {
    let mut store = Store::open_in_memory().unwrap();
    assert_eq!(store.helper_event_seq().unwrap(), 0);
    store.set_helper_event_seq(7).unwrap();
    store
        .transaction(|tx| tx.set_helper_event_seq(u64::MAX))
        .unwrap();
    assert_eq!(store.helper_event_seq().unwrap(), i64::MAX as u64);
    store.set_helper_event_seq(3).unwrap();
    assert_eq!(store.helper_event_seq().unwrap(), 3);
    // The job id counter is untouched.
    assert_eq!(store.next_job_id().unwrap(), JobId(0));
}

// ----- job errors -----

#[test]
fn job_errors_are_kept_without_a_checkpoint_record_and_reported_once() {
    let mut store = Store::open_in_memory().unwrap();
    let a = store
        .insert_job(Uid(1), &spec("a"), &JobState::Checkpointed, 0)
        .unwrap();
    let b = store
        .insert_job(Uid(1), &spec("b"), &JobState::Checkpointed, 0)
        .unwrap();
    let other = store
        .insert_job(Uid(2), &spec("c"), &JobState::Checkpointed, 0)
        .unwrap();
    assert_eq!(store.get_job_error(a.id).unwrap(), None);

    // No checkpoint record is needed.
    store
        .set_job_error(a.id, JobErrorKind::Restore, "criu restore failed", 10)
        .unwrap();
    store
        .set_job_error(other.id, JobErrorKind::Restore, "x", 11)
        .unwrap();
    assert_eq!(
        store.get_job_error(a.id).unwrap(),
        Some(JobError {
            job_id: a.id,
            kind: JobErrorKind::Restore,
            message: "criu restore failed".to_owned(),
            recorded_at: 10,
        })
    );
    assert_eq!(
        store.unacknowledged_job_errors(Uid(1)).unwrap(),
        [(a.id, JobErrorKind::Restore)]
    );
    assert_eq!(
        store.unacknowledged_job_errors(Uid(2)).unwrap(),
        [(other.id, JobErrorKind::Restore)]
    );

    store
        .acknowledge_job_errors(&[a.id, b.id, JobId(99)])
        .unwrap();
    assert!(store.unacknowledged_job_errors(Uid(1)).unwrap().is_empty());

    // The same error again (a replayed event) is not reported again ...
    store
        .set_job_error(a.id, JobErrorKind::Restore, "criu restore failed", 12)
        .unwrap();
    assert!(store.unacknowledged_job_errors(Uid(1)).unwrap().is_empty());
    assert_eq!(store.get_job_error(a.id).unwrap().unwrap().recorded_at, 12);
    // ... a different one is.
    store
        .set_job_error(a.id, JobErrorKind::Restore, "driver mismatch", 13)
        .unwrap();
    assert_eq!(
        store.unacknowledged_job_errors(Uid(1)).unwrap(),
        [(a.id, JobErrorKind::Restore)]
    );

    assert!(store.clear_job_error(a.id).unwrap());
    assert!(!store.clear_job_error(a.id).unwrap());
    assert_eq!(store.get_job_error(a.id).unwrap(), None);

    // Unknown jobs are rejected; removal and switch take the error along.
    assert_domain(
        store
            .set_job_error(JobId(99), JobErrorKind::Restore, "x", 0)
            .unwrap_err(),
        Error::JobNotFound(JobId(99)),
    );
    store.remove_job(other.id).unwrap();
    assert_eq!(count(&store, "job_errors"), 0);

    let q1 = store
        .insert_job(Uid(1), &spec("q1"), &queued(1), 1)
        .unwrap();
    let q2 = store
        .insert_job(Uid(1), &spec("q2"), &queued(1), 1)
        .unwrap();
    store
        .set_job_error(q1.id, JobErrorKind::Restore, "moved", 0)
        .unwrap();
    store
        .transaction(|tx| tx.swap_job_ids(q1.id, q2.id))
        .unwrap();
    assert_eq!(store.get_job_error(q1.id).unwrap(), None);
    assert_eq!(
        store.get_job_error(q2.id).unwrap().unwrap().message,
        "moved"
    );
}

#[test]
fn unknown_job_error_kind_is_reported_as_an_error() {
    let store = Store::open_in_memory().unwrap();
    let job = store.insert_job(Uid(1), &spec("a"), &queued(0), 0).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO job_errors (job_id, kind, message, recorded_at)
             VALUES (?1, 'bogus', 'x', 0)",
            [job.id.0],
        )
        .unwrap();
    assert!(store.get_job_error(job.id).is_err());
}

#[test]
fn version_two_database_moves_restore_errors_out_of_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(migrations::MIGRATIONS[0]).unwrap();
        conn.execute_batch(migrations::MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO jobs (id, owner_uid, command, cwd, group_name, state, state_data,
                                   created_at)
                 VALUES (?1, 1, 'x', x'2f', 'default', ?2, ?3, 0)",
            )
            .unwrap();
        for (id, state) in [
            (0, JobState::Checkpointed),
            (1, JobState::Checkpointing),
            (2, JobState::Checkpointed),
        ] {
            insert
                .execute(rusqlite::params![
                    id,
                    state.name(),
                    serde_json::to_string(&state).unwrap()
                ])
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO checkpoints (job_id, path, created_at, status, failure_reason,
                                      failure_acknowledged)
             VALUES (0, x'2f', 7, 'complete', 'criu restore failed', 0),
                    (1, x'2f', 8, 'failed', 'dump failed', 1),
                    (2, x'2f', 9, 'complete', NULL, 0);",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    // The restore error moved to the job ...
    assert_eq!(
        store.get_job_error(JobId(0)).unwrap(),
        Some(JobError {
            job_id: JobId(0),
            kind: JobErrorKind::Restore,
            message: "criu restore failed".to_owned(),
            recorded_at: 7,
        })
    );
    let record = store.get_checkpoint(JobId(0)).unwrap().unwrap();
    assert_eq!(record.status, CheckpointStatus::Complete);
    assert_eq!(record.failure_reason, None);
    // ... and is reported once, as it never was before.
    assert_eq!(
        store.unacknowledged_job_errors(Uid(1)).unwrap(),
        [(JobId(0), JobErrorKind::Restore)]
    );

    // Failed dumps keep their reason and acknowledgement.
    let failed = store.get_checkpoint(JobId(1)).unwrap().unwrap();
    assert_eq!(failed.failure_reason.as_deref(), Some("dump failed"));
    assert!(
        store
            .unacknowledged_checkpoint_failures(Uid(1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.get_job_error(JobId(1)).unwrap(), None);
    assert_eq!(store.get_job_error(JobId(2)).unwrap(), None);
}

#[test]
fn version_three_database_flattens_job_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    // `state_data` exactly as schema version 3 wrote it.
    let old_rows: &[(&str, &str)] = &[
        ("queued", r#"{"state":"queued","enqueued_at":5}"#),
        ("stashed", r#"{"state":"stashed","enqueue_at":null}"#),
        ("running", r#"{"state":"running"}"#),
        (
            "restore_queued",
            r#"{"state":"restore_queued","enqueued_at":7}"#,
        ),
        ("done", r#"{"state":"done","result":{"result":"success"}}"#),
        (
            "done",
            r#"{"state":"done","result":{"result":"failed","exit_code":-3}}"#,
        ),
        (
            "done",
            r#"{"state":"done","result":{"result":"failed_to_spawn","reason":"no \"such\" file"}}"#,
        ),
        ("done", r#"{"state":"done","result":{"result":"killed"}}"#),
        (
            "done",
            r#"{"state":"done","result":{"result":"dependency_failed"}}"#,
        ),
        (
            "done",
            r#"{"state":"done","result":{"result":"checkpoint_failed","reason":"criu: dump failed"}}"#,
        ),
        ("done", r#"{"state":"done","result":{"result":"lost"}}"#),
        (
            "terminating",
            r#"{"state":"terminating","result":{"result":"checkpoint_failed","reason":"x"}}"#,
        ),
    ];
    {
        let conn = Connection::open(&path).unwrap();
        for sql in &migrations::MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO jobs (id, owner_uid, command, cwd, group_name, state, state_data,
                                   created_at)
                 VALUES (?1, 1, 'x', x'2f', 'default', ?2, ?3, 0)",
            )
            .unwrap();
        for (id, (name, data)) in old_rows.iter().enumerate() {
            insert.execute(rusqlite::params![id, name, data]).unwrap();
        }
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'next_job_id'",
            [old_rows.len()],
        )
        .unwrap();
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let expected = [
        queued(5),
        JobState::Stashed { enqueue_at: None },
        JobState::Running,
        JobState::RestoreQueued { enqueued_at: 7 },
        JobState::Done {
            result: JobResult::Success,
        },
        JobState::Done {
            result: JobResult::Failed { exit_code: -3 },
        },
        JobState::Done {
            result: JobResult::FailedToSpawn {
                reason: "no \"such\" file".to_owned(),
            },
        },
        JobState::Done {
            result: JobResult::Killed,
        },
        JobState::Done {
            result: JobResult::DependencyFailed,
        },
        JobState::Done {
            result: JobResult::CheckpointFailed {
                reason: "criu: dump failed".to_owned(),
            },
        },
        JobState::Done {
            result: JobResult::Lost,
        },
        JobState::Terminating {
            result: JobResult::CheckpointFailed {
                reason: "x".to_owned(),
            },
        },
    ];
    let jobs = store.list_jobs(&JobFilter::all()).unwrap();
    assert_eq!(jobs.len(), expected.len());
    for (job, state) in jobs.iter().zip(&expected) {
        assert_eq!(&job.state, state, "job {}", job.id);
        // The stored JSON is now exactly what this version writes.
        let stored: String = store
            .conn
            .query_row(
                "SELECT state_data FROM jobs WHERE id = ?1",
                [job.id.0],
                |r| r.get(0),
            )
            .unwrap();
        let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(
            stored,
            serde_json::to_value(state).unwrap(),
            "job {}",
            job.id
        );
    }
    // Rows without a nested result are untouched, byte for byte.
    let running: String = store
        .conn
        .query_row("SELECT state_data FROM jobs WHERE id = 2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(running, r#"{"state":"running"}"#);
    assert_eq!(store.next_job_id().unwrap(), JobId(old_rows.len() as u64));
}
