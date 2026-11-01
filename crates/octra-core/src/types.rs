use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceSummary {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteSummary {
    pub id: String,
    pub service_id: String,
    pub host: Option<String>,
    pub path_prefix: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotCounts {
    pub services: i64,
    pub routes: i64,
    pub upstreams: i64,
    pub targets: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub generated_at: DateTime<Utc>,
    pub counts: SnapshotCounts,
}
