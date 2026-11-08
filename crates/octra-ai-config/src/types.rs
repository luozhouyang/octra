//! Entity types stored by the control plane.
//!
//! These records are intentionally flat and reference other records by ID.
//! Referential integrity and ordering are enforced by the compiler, not by the
//! types themselves, so partially built configurations remain representable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// All control-plane records, in storage order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AiControlPlaneData {
    pub providers: Vec<AiProvider>,
    pub accounts: Vec<AiProviderAccount>,
    pub models: Vec<AiModel>,
    pub targets: Vec<AiModelTarget>,
    pub policies: Vec<AiPolicy>,
    pub attachments: Vec<AiPolicyAttachment>,
}

/// Whether a model is a concrete model or a routing group of models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiModelKind {
    Model,
    Group,
}

/// The entity a policy attachment applies to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiPolicyScope {
    Global,
    Provider,
    Model,
    Target,
    ConsumerGroup,
    Consumer,
    VirtualKey,
}

/// The behavior implemented by a policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiPolicyType {
    Allowlist,
    RateLimit,
    BudgetGuard,
    PiiRedaction,
    PromptGuard,
    PromptCompress,
    SemanticCache,
    ModelMap,
    ParamOverride,
    SystemPrompt,
    RequestAudit,
    ThinkTags,
}

/// Upstream vendor endpoint that speaks one wire protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiProvider {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub auth_ref: Option<String>,
    pub capabilities: Vec<String>,
    pub config: Value,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A credential set belonging to a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiProviderAccount {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub credential_kind: String,
    pub secret_ref: Option<String>,
    pub oauth_ref: Option<String>,
    pub weight: i64,
    pub priority: i64,
    pub quota_limit: Option<i64>,
    pub quota_unit: String,
    pub quota_period: String,
    pub quota_used: i64,
    pub cooldown_until: Option<String>,
    pub enabled: bool,
    /// Server-managed OAuth state. Clients cannot set it through account CRUD.
    #[serde(default = "default_oauth_status")]
    pub oauth_status: String,
    /// Server-managed OAuth token expiry, if one was issued.
    #[serde(default)]
    pub oauth_expires_at: Option<String>,
    pub config: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_oauth_status() -> String {
    "none".to_string()
}

/// How a model chooses among its targets.
///
/// `#[serde(default)]` keeps a bare `{}` (the database column default) valid
/// and maps it to the documented priority strategy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AiBalancer {
    pub strategy: String,
    pub retries: u32,
    pub failover_criteria: Vec<String>,
}

impl Default for AiBalancer {
    fn default() -> Self {
        Self {
            strategy: "priority".to_string(),
            retries: 0,
            failover_criteria: Vec::new(),
        }
    }
}

/// A routable model, either concrete or a group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiModel {
    pub id: String,
    pub name: String,
    pub kind: AiModelKind,
    pub capabilities: Vec<String>,
    pub formats: Vec<String>,
    pub route_paths: Vec<String>,
    pub balancer: AiBalancer,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One expansion of a model: a provider endpoint or a nested model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiModelTarget {
    pub id: String,
    pub model_id: String,
    pub target_model_id: Option<String>,
    pub provider_id: Option<String>,
    pub upstream_model: Option<String>,
    pub account_id: Option<String>,
    pub weight: i64,
    pub priority: i64,
    pub position: i64,
    pub config: Value,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i32>,
    /// Capability flags such as {"vision": true, "tools": true, "reasoning": true}.
    #[serde(default = "default_json_object")]
    pub capabilities: Value,
    /// Pricing document such as {"input_per_million": 3.0, "output_per_million": 15.0}.
    #[serde(default = "default_json_object")]
    pub pricing: Value,
    pub semantic_description: Option<String>,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A reusable policy definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiPolicy {
    pub id: String,
    pub name: String,
    pub policy_type: AiPolicyType,
    pub config: Value,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Binds a policy to a scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiPolicyAttachment {
    pub id: String,
    pub policy_id: String,
    pub scope: AiPolicyScope,
    pub scope_id: Option<String>,
    pub order: i32,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}
