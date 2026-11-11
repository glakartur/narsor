//! Backend lease DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controller::Freshness;
use crate::identifier::BackendId;

/// A controller's lease on one backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseView {
    pub backend_id: BackendId,
    pub controller_id: String,
    pub epoch: u64,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcquireLeaseRequest {
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseEpochRequest {
    pub epoch: u64,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// Current lease holder of a task's backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerPresence {
    pub controller_id: String,
    pub lease_epoch: u64,
    pub lease_expires_at: DateTime<Utc>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub freshness: Freshness,
}
