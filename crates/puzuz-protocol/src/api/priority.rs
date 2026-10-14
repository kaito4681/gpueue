//! Priority and fair-share request and response bodies (`puzuz prio`,
//! `share`, `nice`, `top`, `site`, `group priority`).

use puzuz_core::model::{JobId, Timestamp};
use puzuz_core::priority::{JobPriority, PriorityType, ResetPeriod, UserShare};
use serde::{Deserialize, Serialize};

/// Body of `200 OK` from `GET /v1/priority` (`puzuz prio`, like Slurm's
/// `sprio`). Visible to everybody: it names no commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityResponse {
    pub priority_type: PriorityType,
    pub weights: PriorityWeights,
    /// Every pending (queued or restore-queued) job, in the order the
    /// scheduler considers them (highest priority first).
    pub jobs: Vec<JobPriority>,
}

/// The configured weights, for showing what the factors are worth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityWeights {
    pub age: u32,
    pub fairshare: u32,
    pub job_size: u32,
    pub partition: u32,
    /// `priority_weight_tres`, e.g. `"GRES/gpu=1000"`.
    pub tres: String,
}

/// Body of `200 OK` from `GET /v1/shares` (`puzuz share`, like Slurm's
/// `sshare`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharesResponse {
    /// Active users (unfinished jobs or recorded usage) and users with
    /// shares set, ordered by uid.
    pub users: Vec<UserShare>,
    /// `priority_decay_half_life_secs` (0: no decay).
    pub decay_half_life_secs: u64,
    /// `priority_calc_period_secs`.
    pub calc_period_secs: u64,
    pub usage_reset_period: ResetPeriod,
    /// `tres_billing_weights`.
    pub billing_weights: String,
    /// When usage was last calculated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_calc_at: Option<Timestamp>,
}

/// Body of `PUT /v1/shares/{uid}` (administrators only); answered with
/// `204`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSharesRequest {
    pub shares: u32,
}

/// Body of `PUT /v1/groups/{name}/priority` (administrators only);
/// answered with the group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetGroupPriorityRequest {
    pub priority: u32,
}

/// Body of `POST /v1/jobs/nice` (`puzuz nice`): set the nice value of
/// jobs. Owners may set values `>= 0`, administrators any value. Answered
/// with a [`super::BulkJobResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiceRequest {
    pub ids: Vec<JobId>,
    pub nice: i32,
}

/// Body of `POST /v1/jobs/top` (`puzuz top`): move jobs to the top of
/// their owners' queues, the first id first. Answered with a
/// [`super::BulkJobResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopRequest {
    pub ids: Vec<JobId>,
}

/// Body of `POST /v1/jobs/site` (`puzuz site`, administrators only): set
/// the site factor of jobs. Answered with a [`super::BulkJobResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRequest {
    pub ids: Vec<JobId>,
    pub site: i32,
}
