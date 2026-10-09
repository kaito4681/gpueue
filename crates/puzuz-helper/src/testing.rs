//! Fakes for tests (also used by the integration tests in `tests/`).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use puzuz_checkpoint::exec::{StepError, StepOutput};
use puzuz_checkpoint::plan::{Action, Step};
use puzuz_checkpoint::{Executor, layout};

use crate::host::{GpuSnapshot, HostProbe, ProcInfo, UserInfo};

/// A host with configurable users, GPUs and disk space. Every job has a
/// single process: its init (namespace PID 1).
#[derive(Debug, Default)]
pub struct FakeHost {
    users: Mutex<BTreeMap<u32, UserInfo>>,
    gpus: Mutex<GpuSnapshot>,
    available: Mutex<Option<u64>>,
}

impl FakeHost {
    pub fn add_user(&self, uid: u32, name: &str, gid: u32, groups: &[u32]) {
        self.users.lock().unwrap().insert(
            uid,
            UserInfo {
                name: name.into(),
                gid,
                groups: groups.to_vec(),
            },
        );
    }

    pub fn set_gpus(&self, snap: GpuSnapshot) {
        *self.gpus.lock().unwrap() = snap;
    }

    pub fn set_available(&self, bytes: u64) {
        *self.available.lock().unwrap() = Some(bytes);
    }
}

impl HostProbe for FakeHost {
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
        Ok(vec![ProcInfo {
            host_pid: init_pid,
            ns_pid: 1,
            rss_bytes: 1 << 20,
        }])
    }

    fn gpu_snapshot(&self) -> io::Result<GpuSnapshot> {
        Ok(self.gpus.lock().unwrap().clone())
    }

    fn available_bytes(&self, _path: &Path) -> io::Result<u64> {
        Ok(self.available.lock().unwrap().unwrap_or(u64::MAX))
    }

    fn is_tmpfs(&self, _path: &Path) -> io::Result<bool> {
        Ok(false)
    }

    fn resolve_ns_pids(&self, ns_member: u32, wanted: &[u32]) -> Result<Vec<u32>, String> {
        Ok(wanted.iter().map(|w| ns_member + w - 1).collect())
    }

    fn lookup_user(&self, uid: u32) -> io::Result<Option<UserInfo>> {
        Ok(self.users.lock().unwrap().get(&uid).cloned())
    }

    fn ensure_root_bind(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn release_root_bind(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// Shared log and failure switches of [`FakeExecutor`]s.
#[derive(Debug, Default)]
pub struct FakeExecState {
    pub actions: Vec<Option<Action>>,
    /// Fail the first step whose action matches.
    pub fail_on: Option<fn(&Action) -> bool>,
}

/// Executes plans by recording them. `criu dump` creates a minimal image
/// (`inventory.img`) so the checkpoint directory validates.
#[derive(Debug, Clone, Default)]
pub struct FakeExecutor(pub Arc<Mutex<FakeExecState>>);

impl Executor for FakeExecutor {
    fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
        let mut st = self.0.lock().unwrap();
        st.actions.push(step.action());
        if let (Some(action), Some(fail)) = (step.action(), st.fail_on)
            && fail(&action)
        {
            st.fail_on = None;
            return Err(StepError::Failed {
                code: Some(1),
                output: StepOutput {
                    stdout: String::new(),
                    stderr: "injected failure".into(),
                },
            });
        }
        if let Step::Run {
            action: Action::CriuDump { .. },
            command,
            ..
        } = step
            && let Some(dir) = command.value_of("-D")
        {
            fs::write(Path::new(dir).join(layout::CRIU_INVENTORY), b"inv")?;
        }
        Ok(StepOutput::default())
    }
}
