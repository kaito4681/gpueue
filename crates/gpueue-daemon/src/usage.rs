//! Checkpoint image usage versus `checkpoint_size_limit_bytes`.
//!
//! Per the spec, images over the limit are never deleted and dumps are not
//! refused; humans are told instead, through the warnings the API attaches
//! to CLI responses (see [`gpueue_protocol::api::WARNINGS_HEADER`]):
//!
//! - After a successful dump that leaves the images over the limit, the
//!   job's owner gets [`Warning::CheckpointOverLimit`] on their next command,
//!   once per checkpoint.
//! - Administrators (root, `gpueue-admin`) get
//!   [`Warning::CheckpointUsageOverLimit`] on their commands while the images
//!   are over the limit, at most once per
//!   `checkpoint_size_warning_interval_secs` each. Seeing the usage back
//!   under the limit resets every reminder, so the next time it is exceeded
//!   they are told right away. An administrator who just got the owner
//!   warning (which carries the same numbers) is not reminded separately.
//!
//! The usage is summed from the database on each CLI request (only when a
//! limit is configured), so removals and restores are noticed without
//! hooking every path that deletes an image. Pending owner warnings and the
//! reminder times live in memory: a daemon restart may drop an undelivered
//! owner warning, and reminds administrators once more.
//!
//! The daemon also logs a warning at start-up and after each dump while the
//! limit is exceeded.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use gpueue_core::model::{JobId, Uid};
use gpueue_core::storage::{CheckpointStatus, StoreOps};
use gpueue_protocol::api::{CheckpointStorage, Warning};

use crate::error::ApiResult;
use crate::state::Core;

/// In-memory bookkeeping of size warnings.
#[derive(Debug, Default)]
pub struct SizeWarnings {
    /// [`Warning::CheckpointOverLimit`]s not delivered to their owner yet.
    owners: HashMap<Uid, Vec<Warning>>,
    /// When each user last got a size warning since the usage was last seen
    /// under the limit.
    reminded: HashMap<Uid, Instant>,
}

/// Total images over a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverLimit {
    pub total_bytes: u64,
    pub limit_bytes: u64,
}

impl Core {
    /// Number and total size of the complete checkpoint images.
    pub async fn checkpoint_usage(&self) -> ApiResult<(u64, u64)> {
        let records = self.read(|s| s.list_checkpoints()).await?;
        Ok(records
            .iter()
            .filter(|r| r.status == CheckpointStatus::Complete)
            .fold((0, 0), |(count, total), r| {
                (count + 1, total.saturating_add(r.size_bytes.unwrap_or(0)))
            }))
    }

    /// The usage summary `GET /v1/version` reports.
    pub async fn checkpoint_storage(&self) -> ApiResult<CheckpointStorage> {
        let (images, total_bytes) = self.checkpoint_usage().await?;
        let dir = self.config.checkpoints_dir();
        let available_bytes = tokio::task::spawn_blocking(move || available_bytes(&dir))
            .await
            .ok()
            .flatten();
        Ok(CheckpointStorage {
            images,
            total_bytes,
            limit_bytes: self.config.checkpoint_size_limit_bytes,
            available_bytes,
        })
    }

    /// Whether the complete images exceed `checkpoint_size_limit_bytes`;
    /// `None` without a limit, when within it, or when the usage cannot be
    /// read.
    async fn over_limit(&self) -> Option<OverLimit> {
        let limit_bytes = self.config.checkpoint_size_limit_bytes?;
        match self.checkpoint_usage().await {
            Ok((_, total_bytes)) => (total_bytes > limit_bytes).then_some(OverLimit {
                total_bytes,
                limit_bytes,
            }),
            Err(e) => {
                tracing::warn!("cannot sum up checkpoint sizes: {}", e.0.message);
                None
            }
        }
    }

    /// Logs a warning when the complete images exceed
    /// `checkpoint_size_limit_bytes`, and returns the excess.
    pub async fn check_checkpoint_usage(&self) -> Option<OverLimit> {
        let over = self.over_limit().await?;
        tracing::warn!(
            total_bytes = over.total_bytes,
            limit_bytes = over.limit_bytes,
            dir = %self.config.checkpoints_dir().display(),
            "checkpoint images exceed checkpoint_size_limit_bytes; they are not deleted \
             automatically (restore or remove checkpointed jobs to free space)"
        );
        Some(over)
    }

    /// Called after `job` was checkpointed successfully: when that left the
    /// images over the limit, logs it and queues a warning for the owner.
    pub async fn note_new_checkpoint(&self, job: JobId) {
        let Some(over) = self.check_checkpoint_usage().await else {
            return;
        };
        let owner = match self.read(move |s| s.get_job(job)).await {
            Ok(Some(job)) => job.owner,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(job = %job, "cannot look up the owner: {}", e.0.message);
                return;
            }
        };
        let warning = Warning::CheckpointOverLimit {
            job,
            total_bytes: over.total_bytes,
            limit_bytes: over.limit_bytes,
        };
        let mut state = self.size_warnings.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state.owners.entry(owner).or_default();
        // A newer checkpoint of the same job replaces the older warning.
        pending.retain(|w| w.job() != Some(job));
        pending.push(warning);
    }

    /// The size warnings `uid` should see now (see the module docs).
    pub async fn pending_size_warnings(&self, uid: Uid) -> Vec<Warning> {
        let mut warnings = {
            let state = self.size_warnings.lock().unwrap_or_else(|e| e.into_inner());
            state.owners.get(&uid).cloned().unwrap_or_default()
        };
        if self.config.checkpoint_size_limit_bytes.is_none() {
            return warnings;
        }
        let Some(over) = self.over_limit().await else {
            let mut state = self.size_warnings.lock().unwrap_or_else(|e| e.into_inner());
            state.reminded.clear();
            return warnings;
        };
        if !warnings.is_empty() || self.reminded_recently(uid) {
            return warnings;
        }
        let users = self.users.clone();
        let is_admin = tokio::task::spawn_blocking(move || users.is_admin(uid))
            .await
            .unwrap_or(false);
        if is_admin {
            warnings.push(Warning::CheckpointUsageOverLimit {
                total_bytes: over.total_bytes,
                limit_bytes: over.limit_bytes,
            });
        }
        warnings
    }

    fn reminded_recently(&self, uid: Uid) -> bool {
        let interval = Duration::from_secs(self.config.checkpoint_size_warning_interval_secs);
        let state = self.size_warnings.lock().unwrap_or_else(|e| e.into_inner());
        state
            .reminded
            .get(&uid)
            .is_some_and(|at| at.elapsed() < interval)
    }

    /// Records that the size warnings among `delivered` reached `uid`.
    pub fn acknowledge_size_warnings(&self, uid: Uid, delivered: &[Warning]) {
        let is_size_warning = |w: &Warning| {
            matches!(
                w,
                Warning::CheckpointOverLimit { .. } | Warning::CheckpointUsageOverLimit { .. }
            )
        };
        if !delivered.iter().any(is_size_warning) {
            return;
        }
        let mut state = self.size_warnings.lock().unwrap_or_else(|e| e.into_inner());
        state.reminded.insert(uid, Instant::now());
        if let Some(pending) = state.owners.get_mut(&uid) {
            pending.retain(|w| !delivered.contains(w));
            if pending.is_empty() {
                state.owners.remove(&uid);
            }
        }
    }
}

/// Free space for unprivileged users on the filesystem holding `dir`.
fn available_bytes(dir: &Path) -> Option<u64> {
    let st = nix::sys::statvfs::statvfs(dir).ok()?;
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (blocks, size) = (st.blocks_available() as u64, st.fragment_size() as u64);
    Some(blocks.saturating_mul(size))
}
