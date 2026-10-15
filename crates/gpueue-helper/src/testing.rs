//! Fakes for tests (also used by the integration tests in `tests/`).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use gpueue_checkpoint::exec::{StepError, StepOutput};
use gpueue_checkpoint::plan::{Action, Step};
use gpueue_checkpoint::{Executor, layout};

use crate::access::{VerifyReply, VerifyRequest};
use crate::host::{DumpHazards, GpuSnapshot, HostProbe, ProcInfo, UserInfo};

/// A host with configurable users, GPUs and disk space. Every job has a
/// single process: its init (namespace PID 1).
#[derive(Debug, Default)]
pub struct FakeHost {
    users: Mutex<BTreeMap<u32, UserInfo>>,
    gpus: Mutex<GpuSnapshot>,
    /// See [`FakeHost::set_gpu_minors`].
    minors: Mutex<Option<Vec<u32>>>,
    available: Mutex<Option<u64>>,
    cuda: Mutex<Vec<u32>>,
    mounts: Mutex<Vec<String>>,
    unreachable: Mutex<Vec<String>>,
    hazards: Mutex<DumpHazards>,
    groups: Mutex<Option<Vec<u32>>>,
    path_problems: Mutex<Vec<String>>,
    restored_problems: Mutex<Vec<String>>,
    verified: Mutex<Vec<VerifyRequest>>,
    tmpfs_bytes: Mutex<u64>,
    dead: Mutex<crate::deadmounts::DeadMounts>,
    /// See [`FakeHost::set_nested_namespace_processes`].
    nested: Mutex<Vec<String>>,
    /// See [`FakeHost::set_bind_source_problems`].
    bind_problems: Mutex<Vec<(String, String)>>,
    /// The CUDA PIDs of every [`HostProbe::with_cuda_threads_running`].
    cuda_released: Mutex<Vec<Vec<u32>>>,
    /// What [`HostProbe::unstopped_threads`] reports.
    unstopped: Mutex<Vec<(u32, u32)>>,
    /// While set, access checks block (see [`FakeHost::hold_access_checks`]).
    held: Mutex<bool>,
    /// See [`FakeHost::set_held_attrs_failure`].
    held_attrs_failure: Mutex<Option<String>>,
    released: std::sync::Condvar,
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

    /// Minor numbers of the GPUs' device nodes by index. Unset: GPU index
    /// `i` has minor `i`, for the GPUs of [`FakeHost::set_gpus`].
    pub fn set_gpu_minors(&self, minors: Vec<u32>) {
        *self.minors.lock().unwrap() = Some(minors);
    }

    /// Host PIDs reported as using CUDA.
    pub fn set_cuda_pids(&self, pids: Vec<u32>) {
        *self.cuda.lock().unwrap() = pids;
    }

    /// Mount points reported for every job.
    pub fn set_mount_points(&self, mounts: Vec<String>) {
        *self.mounts.lock().unwrap() = mounts;
    }

    /// Mounts reported as depending on a hidden mount, for every job.
    pub fn set_unreachable_mounts(&self, mounts: Vec<String>) {
        *self.unreachable.lock().unwrap() = mounts;
    }

    pub fn set_available(&self, bytes: u64) {
        *self.available.lock().unwrap() = Some(bytes);
    }

    /// What [`HostProbe::private_tmpfs_bytes`] reports for every job.
    pub fn set_private_tmpfs_bytes(&self, bytes: u64) {
        *self.tmpfs_bytes.lock().unwrap() = bytes;
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

    fn gpu_device_minors(&self) -> io::Result<Vec<u32>> {
        if let Some(minors) = self.minors.lock().unwrap().clone() {
            return Ok(minors);
        }
        let count = self
            .gpus
            .lock()
            .unwrap()
            .gpus
            .iter()
            .map(|g| g.index + 1)
            .max();
        Ok((0..count.unwrap_or(0)).collect())
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

    fn cuda_pids(&self, _host_pids: &[u32]) -> io::Result<Vec<u32>> {
        Ok(self.cuda.lock().unwrap().clone())
    }

    fn mount_points(&self, _pid: u32) -> io::Result<Vec<String>> {
        Ok(self.mounts.lock().unwrap().clone())
    }

    fn unreachable_mounts(&self, _pid: u32) -> io::Result<Vec<String>> {
        Ok(self.unreachable.lock().unwrap().clone())
    }

    fn dump_hazards(&self, _host_pids: &[u32], _private: &[String]) -> io::Result<DumpHazards> {
        Ok(self.hazards.lock().unwrap().clone())
    }

    fn process_groups(&self, _host_pids: &[u32]) -> io::Result<Option<Vec<u32>>> {
        Ok(self.groups.lock().unwrap().clone())
    }

    fn verify_access(&self, req: &VerifyRequest) -> VerifyReply {
        self.verified.lock().unwrap().push(req.clone());
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
        drop(held);
        let problems = match req {
            VerifyRequest::Paths(_) => &self.path_problems,
            VerifyRequest::Restored(_) => &self.restored_problems,
            VerifyRequest::Probe(c) | VerifyRequest::BindSources(c) => {
                return Ok(vec![String::new(); c.paths.len()]);
            }
            VerifyRequest::HeldAttrs(_) => return Err("not a check with a list of problems".into()),
            VerifyRequest::Exist(c) => {
                return Ok(c
                    .paths
                    .iter()
                    .filter(|p| fs::symlink_metadata(p).is_err())
                    .cloned()
                    .collect());
            }
        };
        Ok(problems.lock().unwrap().clone())
    }

    fn private_tmpfs_bytes(&self, _pid: u32, _private: &[String]) -> io::Result<u64> {
        Ok(*self.tmpfs_bytes.lock().unwrap())
    }

    fn held_file_attrs(
        &self,
        files: &[crate::access::HeldFile],
    ) -> io::Result<Vec<Option<crate::access::FileAttrs>>> {
        if let Some(reason) = self.held_attrs_failure.lock().unwrap().clone() {
            return Err(io::Error::other(reason));
        }
        Ok(files.iter().map(crate::access::held_file_attrs).collect())
    }

    fn dead_mounts(
        &self,
        _init_pid: u32,
        _host_pids: &[u32],
    ) -> io::Result<crate::deadmounts::DeadMounts> {
        Ok(self.dead.lock().unwrap().clone())
    }

    fn nested_namespace_processes(&self, _init_pid: u32) -> io::Result<Vec<String>> {
        Ok(self.nested.lock().unwrap().clone())
    }

    fn bind_source_problems(&self, paths: &[String]) -> io::Result<Vec<Option<String>>> {
        let problems = self.bind_problems.lock().unwrap();
        Ok(paths
            .iter()
            .map(|p| {
                problems
                    .iter()
                    .find(|(q, _)| q == p)
                    .map(|(_, why)| why.clone())
            })
            .collect())
    }

    fn with_cuda_threads_running(
        &self,
        _init_pid: u32,
        cuda_pids: &[u32],
        restore: &mut (dyn FnMut() -> Result<(), String> + Send),
    ) -> Result<(), String> {
        self.cuda_released.lock().unwrap().push(cuda_pids.to_vec());
        restore()
    }

    fn unstopped_threads(&self, _init_pid: u32) -> io::Result<Vec<(u32, u32)>> {
        Ok(self.unstopped.lock().unwrap().clone())
    }
}

impl FakeHost {
    /// What [`HostProbe::dump_hazards`] reports for every job.
    pub fn set_dump_hazards(&self, h: DumpHazards) {
        *self.hazards.lock().unwrap() = h;
    }

    /// What [`HostProbe::dead_mounts`] reports for every job.
    pub fn set_dead_mounts(&self, d: crate::deadmounts::DeadMounts) {
        *self.dead.lock().unwrap() = d;
    }

    /// What [`HostProbe::nested_namespace_processes`] reports for every
    /// job.
    pub fn set_nested_namespace_processes(&self, procs: Vec<String>) {
        *self.nested.lock().unwrap() = procs;
    }

    /// (path, why) that [`HostProbe::bind_source_problems`] reports; every
    /// other path may be bound.
    pub fn set_bind_source_problems(&self, problems: Vec<(String, String)>) {
        *self.bind_problems.lock().unwrap() = problems;
    }

    /// What [`HostProbe::process_groups`] reports for every job.
    pub fn set_process_groups(&self, g: Option<Vec<u32>>) {
        *self.groups.lock().unwrap() = g;
    }

    /// Problems the access checks report (before and after a restore).
    pub fn set_access_problems(&self, paths: Vec<String>, restored: Vec<String>) {
        *self.path_problems.lock().unwrap() = paths;
        *self.restored_problems.lock().unwrap() = restored;
    }

    /// Make access checks block until called again with `false` (like a
    /// check stuck on a filesystem that does not answer).
    pub fn hold_access_checks(&self, hold: bool) {
        *self.held.lock().unwrap() = hold;
        self.released.notify_all();
    }

    /// Every access check requested so far.
    pub fn verified(&self) -> Vec<VerifyRequest> {
        self.verified.lock().unwrap().clone()
    }

    /// The CUDA PIDs of every restore of a stopped job's CUDA state (see
    /// [`HostProbe::with_cuda_threads_running`]).
    pub fn cuda_released(&self) -> Vec<Vec<u32>> {
        self.cuda_released.lock().unwrap().clone()
    }

    /// Threads reported as not stopped (see
    /// [`HostProbe::unstopped_threads`]).
    pub fn set_unstopped_threads(&self, threads: Vec<(u32, u32)>) {
        *self.unstopped.lock().unwrap() = threads;
    }

    /// Make [`HostProbe::held_file_attrs`] fail with `reason` (as the real
    /// host's lookup child does when it cannot be run or does not finish
    /// in time); `None` to look the files up again.
    pub fn set_held_attrs_failure(&self, reason: Option<&str>) {
        *self.held_attrs_failure.lock().unwrap() = reason.map(str::to_owned);
    }
}

/// Shared log and failure switches of [`FakeExecutor`]s.
#[derive(Debug, Default)]
pub struct FakeExecState {
    pub actions: Vec<Option<Action>>,
    /// Fail the first step whose action matches.
    pub fail_on: Option<fn(&Action) -> bool>,
    /// Let the first step whose action matches hang until its timeout,
    /// then fail as killed (as a real stuck tool would).
    pub hang_on: Option<fn(&Action) -> bool>,
    /// Roots of the [`Step::Thaw`]s run.
    pub thawed: Vec<u32>,
    /// Called with the images directory after each successful `criu dump`.
    pub after_dump: Option<fn(&Path)>,
    /// What the `post-dump` hook of each successful `criu dump`
    /// (`--action-script`, see [`crate::access::run_dump_hook`]) reports.
    pub dump_hook_held: Option<Vec<crate::access::HeldFile>>,
    /// The arguments of every `criu dump` run.
    pub dump_args: Vec<Vec<String>>,
}

/// Executes plans by recording them. `criu dump` creates a minimal image
/// (`inventory.img`) so the checkpoint directory validates.
#[derive(Debug, Clone, Default)]
pub struct FakeExecutor(pub Arc<Mutex<FakeExecState>>);

impl Executor for FakeExecutor {
    fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
        let mut st = self.0.lock().unwrap();
        st.actions.push(step.action());
        if let Step::Thaw { root_pid } = step {
            st.thawed.push(*root_pid);
        }
        // CRIU writes its log whether or not the dump succeeds.
        if let Step::Run {
            action: Action::CriuDump { .. },
            command,
            ..
        } = step
        {
            st.dump_args.push(command.args_lossy());
        }
        if let Step::Run {
            action: Action::CriuDump { .. },
            command,
            ..
        } = step
            && let Some(log) = command.value_of("-o")
        {
            fs::write(log, b"(00.000001) Error (criu/fake.c:1): fake criu log\n")?;
        }
        if let (
            Step::Run {
                action, timeout, ..
            },
            Some(hang),
        ) = (step, st.hang_on)
            && hang(action)
        {
            st.hang_on = None;
            drop(st);
            std::thread::sleep(*timeout);
            return Err(StepError::TimedOut {
                after: *timeout,
                output: StepOutput::default(),
            });
        }
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
            if let (Some(held), Some(script)) =
                (&st.dump_hook_held, command.value_of("--action-script"))
            {
                let script = fs::read_to_string(script)?;
                let request = script
                    .trim_end()
                    .rsplit_once(" dump-hook ")
                    .map(|(_, r)| r.trim_matches('\''))
                    .ok_or_else(|| io::Error::other(format!("unknown action script {script}")))?;
                fs::write(
                    crate::access::hook_result_path(Path::new(request)),
                    serde_json::to_vec(held).map_err(io::Error::other)?,
                )?;
            }
            if let Some(after) = st.after_dump {
                after(Path::new(dir));
            }
        }
        Ok(StepOutput::default())
    }
}
