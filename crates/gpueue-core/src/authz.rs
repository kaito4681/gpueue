//! Authorization rules for the shared, system-wide daemon.
//!
//! - A job's owner and admins (root and members of `gpueue-admin`) may mutate
//!   it (kill, pause, start, remove, checkpoint, restore, restart, ...).
//! - Some operations affect everybody and are admin-only, see
//!   [`AdminOperation`].
//! - Everybody can see that a job exists and who runs it on which GPUs, but
//!   only the owner and admins see its command, cwd, environment, label,
//!   failure details and logs. See [`JobView`].
//!
//! The [`Caller`] always comes from `SO_PEERCRED`, never from the request.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::model::{Caller, GpuIndex, Job, JobId, JobResult, JobState, Timestamp, Uid};

/// Whether `caller` may change `job`.
pub fn can_mutate(caller: &Caller, job: &Job) -> bool {
    caller.is_admin || caller.uid == job.owner
}

/// [`can_mutate`] as a `Result`, for use with `?`.
pub fn require_mutate(caller: &Caller, job: &Job) -> Result<(), Error> {
    if can_mutate(caller, job) {
        Ok(())
    } else {
        Err(Error::PermissionDenied)
    }
}

/// Whether `caller` may see `job`'s private details (command, cwd, envs,
/// label, failure reasons) and read its log.
pub fn can_view_details(caller: &Caller, job: &Job) -> bool {
    can_mutate(caller, job)
}

/// Whether `caller` may read or follow `job`'s log.
pub fn can_read_log(caller: &Caller, job: &Job) -> bool {
    can_view_details(caller, job)
}

/// Operations that affect every user and are therefore admin-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOperation {
    CreateGroup,
    RemoveGroup,
    /// `parallel <N> -g <group>`.
    SetGroupParallel,
    /// `gpu parallel <N>`.
    SetGpuParallel,
    /// Pausing or resuming a whole group (`pause -g`, `start -g`, and
    /// `kill -g`, which pauses the group as in pueue). Non-admins applying
    /// such a command to a group only act on their own jobs in it and leave
    /// the group's status alone.
    SetGroupStatus,
    /// `shutdown`.
    Shutdown,
    /// `reset` of other users' jobs; a plain `reset` by a normal user only
    /// touches their own jobs (see [`reset_scope`]).
    ResetAll,
    /// `share set`: a user's fair-share shares.
    SetShares,
    /// `group priority`: a group's partition priority.
    SetGroupPriority,
    /// `site`: a job's site factor.
    SetSiteFactor,
}

/// Whether `caller` may change a nice value from `current` to `nice` (a
/// new job starts at 0). As in Slurm, users may only raise it (lowering
/// their own priority), never below 0; administrators may set any value.
pub fn require_nice_value(caller: &Caller, current: i32, nice: i32) -> Result<(), Error> {
    if caller.is_admin {
        return Ok(());
    }
    if nice < 0 {
        return Err(Error::Forbidden(
            "only administrators may set a negative nice value".to_owned(),
        ));
    }
    if nice < current {
        return Err(Error::Forbidden(format!(
            "only administrators may lower a nice value (it is {current})"
        )));
    }
    Ok(())
}

/// Whether `caller` may set `job`'s nice value to `nice`: the owner may
/// raise it ([`require_nice_value`]); administrators may set any value on
/// any job.
pub fn require_nice(caller: &Caller, job: &Job, nice: i32) -> Result<(), Error> {
    require_mutate(caller, job)?;
    require_nice_value(caller, job.spec.nice, nice)
}

/// Whether `caller` may move `job` to the top of its owner's queue (`gpueue
/// top`, Slurm's `scontrol top`): administrators always, the owner when
/// `enable_user_top` is set.
pub fn require_top(caller: &Caller, job: &Job, enable_user_top: bool) -> Result<(), Error> {
    if caller.is_admin {
        return Ok(());
    }
    if caller.uid != job.owner {
        return Err(Error::PermissionDenied);
    }
    if !enable_user_top {
        return Err(Error::Forbidden(
            "moving jobs to the top is disabled for users (enable_user_top = false)".to_owned(),
        ));
    }
    Ok(())
}

pub fn require_admin(caller: &Caller) -> Result<(), Error> {
    if caller.is_admin {
        Ok(())
    } else {
        Err(Error::PermissionDenied)
    }
}

/// Check an admin-only operation. Every [`AdminOperation`] currently requires
/// admin rights; the parameter documents the call site and leaves room for
/// finer rules.
pub fn require_admin_for(caller: &Caller, _operation: AdminOperation) -> Result<(), Error> {
    require_admin(caller)
}

/// The set of jobs a bulk selection covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only jobs owned by this user.
    Owner(Uid),
    /// Every job.
    All,
}

impl Scope {
    pub fn includes(&self, job: &Job) -> bool {
        match self {
            Scope::Owner(uid) => job.owner == *uid,
            Scope::All => true,
        }
    }
}

/// Scope of `-a/--all` and `-g/--group` selections for mutating commands:
/// a normal user's own jobs, or every job for an admin.
pub fn selection_scope(caller: &Caller) -> Scope {
    if caller.is_admin {
        Scope::All
    } else {
        Scope::Owner(caller.uid)
    }
}

/// Scope of `reset`: own jobs for normal users, everything for admins.
pub fn reset_scope(caller: &Caller) -> Scope {
    selection_scope(caller)
}

/// Placeholder for failure reasons hidden from non-owners.
pub const REDACTED: &str = "<redacted>";

/// Details only the owner and admins may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDetails {
    pub command: String,
    pub cwd: PathBuf,
    pub envs: BTreeMap<String, String>,
    pub label: Option<String>,
    pub no_checkpoint: bool,
}

/// A job as shown to a particular caller.
///
/// The public part answers "who is using which GPU, and in what state";
/// `details` is `None` for other users' jobs. Label is treated as private:
/// it is free text chosen by the owner and often names the experiment.
/// Failure reasons in the state (spawn errors, CRIU messages) may contain
/// paths or command fragments, so they are replaced by [`REDACTED`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub owner: Uid,
    pub state: JobState,
    pub group: String,
    pub gpus: Vec<GpuIndex>,
    pub dependencies: Vec<JobId>,
    pub priority: i32,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub details: Option<JobDetails>,
}

impl JobView {
    /// Whether this view carries the private details.
    pub fn is_redacted(&self) -> bool {
        self.details.is_none()
    }
}

/// Build the view of `job` that `caller` is allowed to see.
pub fn view(caller: &Caller, job: &Job) -> JobView {
    let full = can_view_details(caller, job);
    let details = full.then(|| JobDetails {
        command: job.spec.command.clone(),
        cwd: job.spec.cwd.clone(),
        envs: job.spec.envs.clone(),
        label: job.spec.label.clone(),
        no_checkpoint: job.spec.no_checkpoint,
    });
    JobView {
        id: job.id,
        owner: job.owner,
        state: if full {
            job.state.clone()
        } else {
            redact_state(&job.state)
        },
        group: job.spec.group.clone(),
        gpus: job.spec.gpus.clone(),
        dependencies: job.spec.dependencies.clone(),
        priority: job.spec.priority,
        created_at: job.created_at,
        started_at: job.started_at,
        finished_at: job.finished_at,
        details,
    }
}

/// Strip free-text failure reasons from a state; everything else is public.
pub fn redact_state(state: &JobState) -> JobState {
    match state {
        JobState::Done { result } => JobState::Done {
            result: redact_result(result),
        },
        JobState::Terminating { result } => JobState::Terminating {
            result: redact_result(result),
        },
        other => other.clone(),
    }
}

fn redact_result(result: &JobResult) -> JobResult {
    match result {
        JobResult::FailedToSpawn { .. } => JobResult::FailedToSpawn {
            reason: REDACTED.to_owned(),
        },
        JobResult::CheckpointFailed { .. } => JobResult::CheckpointFailed {
            reason: REDACTED.to_owned(),
        },
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DEFAULT_GROUP, JobSpec};

    const OWNER: Uid = Uid(1000);
    const OTHER: Uid = Uid(1001);

    fn user(uid: Uid) -> Caller {
        Caller {
            uid,
            is_admin: false,
        }
    }

    fn admin() -> Caller {
        Caller {
            uid: Uid(0),
            is_admin: true,
        }
    }

    fn job(state: JobState) -> Job {
        Job {
            id: JobId(3),
            owner: OWNER,
            spec: JobSpec {
                command: "python train.py --secret-token abc".to_owned(),
                cwd: PathBuf::from("/home/alice/project"),
                envs: BTreeMap::from([("API_KEY".to_owned(), "hunter2".to_owned())]),
                group: DEFAULT_GROUP.to_owned(),
                gpus: vec![GpuIndex(0), GpuIndex(1)],
                dependencies: vec![JobId(1)],
                priority: 2,
                label: Some("resnet-sweep".to_owned()),
                no_checkpoint: true,
                nice: 0,
            },
            state,
            created_at: 10,
            started_at: Some(20),
            finished_at: None,
            site_factor: 0,
            top_seq: None,
        }
    }

    #[test]
    fn owner_and_admin_may_mutate() {
        let j = job(JobState::Running);
        assert!(can_mutate(&user(OWNER), &j));
        assert!(can_mutate(&admin(), &j));
        // A non-root admin from the gpueue-admin group.
        assert!(can_mutate(
            &Caller {
                uid: OTHER,
                is_admin: true
            },
            &j
        ));
        assert!(!can_mutate(&user(OTHER), &j));
        assert_eq!(require_mutate(&user(OWNER), &j), Ok(()));
        assert_eq!(
            require_mutate(&user(OTHER), &j),
            Err(Error::PermissionDenied)
        );
    }

    #[test]
    fn root_is_not_special_without_admin_flag() {
        // Admin status is decided by whoever builds the Caller (root or the
        // gpueue-admin group); this module only trusts the flag.
        let j = job(JobState::Running);
        assert!(!can_mutate(&user(Uid(0)), &j));
    }

    #[test]
    fn logs_follow_detail_visibility() {
        let j = job(JobState::Running);
        assert!(can_read_log(&user(OWNER), &j));
        assert!(can_read_log(&admin(), &j));
        assert!(!can_read_log(&user(OTHER), &j));
        assert!(!can_view_details(&user(OTHER), &j));
    }

    #[test]
    fn admin_only_operations() {
        let ops = [
            AdminOperation::CreateGroup,
            AdminOperation::RemoveGroup,
            AdminOperation::SetGroupParallel,
            AdminOperation::SetGpuParallel,
            AdminOperation::SetGroupStatus,
            AdminOperation::Shutdown,
            AdminOperation::ResetAll,
        ];
        for op in ops {
            assert_eq!(require_admin_for(&admin(), op), Ok(()));
            assert_eq!(
                require_admin_for(&user(OWNER), op),
                Err(Error::PermissionDenied),
                "{op:?}"
            );
        }
        assert_eq!(require_admin(&user(OWNER)), Err(Error::PermissionDenied));
    }

    #[test]
    fn all_scope_is_own_jobs_for_users_and_everything_for_admins() {
        let mine = job(JobState::Running);
        let mut theirs = job(JobState::Running);
        theirs.owner = OTHER;

        let scope = selection_scope(&user(OWNER));
        assert_eq!(scope, Scope::Owner(OWNER));
        assert!(scope.includes(&mine));
        assert!(!scope.includes(&theirs));

        let scope = selection_scope(&admin());
        assert_eq!(scope, Scope::All);
        assert!(scope.includes(&mine) && scope.includes(&theirs));

        assert_eq!(reset_scope(&user(OTHER)), Scope::Owner(OTHER));
        assert_eq!(reset_scope(&admin()), Scope::All);
    }

    #[test]
    fn owner_and_admin_see_everything() {
        let j = job(JobState::Done {
            result: JobResult::FailedToSpawn {
                reason: "cannot chdir to /home/alice/project".to_owned(),
            },
        });
        for caller in [user(OWNER), admin()] {
            let v = view(&caller, &j);
            assert!(!v.is_redacted());
            assert_eq!(v.state, j.state);
            let d = v.details.unwrap();
            assert_eq!(d.command, j.spec.command);
            assert_eq!(d.cwd, j.spec.cwd);
            assert_eq!(d.envs, j.spec.envs);
            assert_eq!(d.label, j.spec.label);
            assert!(d.no_checkpoint);
        }
    }

    #[test]
    fn others_see_public_fields_only() {
        let j = job(JobState::Running);
        let v = view(&user(OTHER), &j);
        assert!(v.is_redacted());
        assert_eq!(v.id, j.id);
        assert_eq!(v.owner, OWNER);
        assert_eq!(v.state, JobState::Running);
        assert_eq!(v.group, DEFAULT_GROUP);
        assert_eq!(v.gpus, vec![GpuIndex(0), GpuIndex(1)]);
        assert_eq!(v.dependencies, vec![JobId(1)]);
        assert_eq!(v.priority, 2);
        assert_eq!(
            (v.created_at, v.started_at, v.finished_at),
            (10, Some(20), None)
        );

        // Debug output contains every field, so a secret missing from it
        // cannot be serialized either.
        let dump = format!("{v:?}");
        for secret in ["train.py", "/home/alice", "hunter2", "API_KEY", "resnet"] {
            assert!(!dump.contains(secret), "{secret} leaked: {dump}");
        }
    }

    #[test]
    fn failure_reasons_are_redacted_for_others() {
        for result in [
            JobResult::FailedToSpawn {
                reason: "/home/alice/secret: not found".to_owned(),
            },
            JobResult::CheckpointFailed {
                reason: "criu: /home/alice/secret".to_owned(),
            },
        ] {
            let j = job(JobState::Done { result });
            let v = view(&user(OTHER), &j);
            let JobState::Done { result } = &v.state else {
                panic!("state changed: {:?}", v.state);
            };
            assert_eq!(std::mem::discriminant(result), {
                let JobState::Done { result: orig } = &j.state else {
                    unreachable!()
                };
                std::mem::discriminant(orig)
            });
            assert!(!format!("{result:?}").contains("alice"));
            assert!(format!("{result:?}").contains(REDACTED));
        }

        let failed = JobState::Done {
            result: JobResult::Failed { exit_code: 3 },
        };
        assert_eq!(redact_state(&failed), failed);
        assert_eq!(
            redact_state(&JobState::Checkpointed),
            JobState::Checkpointed
        );

        let terminating = JobState::Terminating {
            result: JobResult::CheckpointFailed {
                reason: "criu: /home/alice/secret".to_owned(),
            },
        };
        let redacted = format!("{:?}", redact_state(&terminating));
        assert!(!redacted.contains("alice"));
        assert!(redacted.contains(REDACTED));
    }
    #[test]
    fn nice_rules() {
        let j = job(JobState::Queued { enqueued_at: 0 });
        // The owner may only lower the priority.
        assert!(require_nice(&user(OWNER), &j, 0).is_ok());
        assert!(require_nice(&user(OWNER), &j, 100).is_ok());
        assert!(matches!(
            require_nice(&user(OWNER), &j, -1),
            Err(Error::Forbidden(m)) if m.contains("negative")
        ));
        // ... and not undo a nice value an administrator set.
        let mut niced = j.clone();
        niced.spec.nice = 10_000;
        assert!(matches!(
            require_nice(&user(OWNER), &niced, 0),
            Err(Error::Forbidden(m)) if m.contains("10000")
        ));
        assert!(require_nice(&user(OWNER), &niced, 10_000).is_ok());
        assert!(require_nice(&user(OWNER), &niced, 20_000).is_ok());
        assert!(require_nice(&admin(), &niced, 0).is_ok());
        assert!(require_nice_value(&user(OWNER), 0, 5).is_ok());
        // Others may not touch it at all.
        assert_eq!(
            require_nice(&user(OTHER), &j, 5),
            Err(Error::PermissionDenied)
        );
        // Administrators may set anything.
        assert!(require_nice(&admin(), &j, -1000).is_ok());
    }

    #[test]
    fn top_rules() {
        let j = job(JobState::Queued { enqueued_at: 0 });
        assert!(require_top(&user(OWNER), &j, true).is_ok());
        assert!(matches!(
            require_top(&user(OWNER), &j, false),
            Err(Error::Forbidden(m)) if m.contains("enable_user_top")
        ));
        assert_eq!(
            require_top(&user(OTHER), &j, true),
            Err(Error::PermissionDenied)
        );
        assert!(require_top(&admin(), &j, false).is_ok());
    }
}
