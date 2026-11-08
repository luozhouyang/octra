//! Persisted MCP Gateway entities and their default ACL policy.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::protocol::Transport;

/// Invocation lifecycle statuses stored in `mcp_invocations.status`.
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const TIMEOUT: &str = "timeout";
    pub const DENIED: &str = "denied";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID: &str = "invalid";
}

/// `mcp_sessions.status` values. A terminated session keeps its row for audit
/// and answers later requests on the same id with "session not found".
pub mod session_status {
    pub const OPEN: &str = "open";
    pub const TERMINATED: &str = "terminated";
}

/// The ACL policy when a tool has no `mcp_tool_acls` rows at all.
///
/// Octra ships open-local by default: a tool without explicit rows is callable
/// by every authenticated (or, when no API keys exist, anonymous) consumer.
/// Flip this to `false` to make that case default-deny instead; tools with
/// rows are always governed by those rows either way.
pub const ACL_DEFAULT_ALLOW: bool = true;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub transport: Transport,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub headers: Map<String, Value>,
    pub protocol_version: Option<String>,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub transport: Transport,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: Value,
    /// Header values may be `<scheme>:<reference>` secret references; they are
    /// never stored in plaintext.
    pub headers: Map<String, Value>,
}

impl NewMcpServer {
    /// Ensures the transport has the fields it needs and no extra ones.
    pub fn validate(&self) -> Result<(), String> {
        crate::protocol::is_valid_server_name(&self.name)
            .then_some(())
            .ok_or_else(|| format!("invalid server name: {}", self.name))?;

        match self.transport {
            Transport::Stdio => {
                let command = self.command.as_deref().unwrap_or_default().trim();
                if command.is_empty() {
                    return Err("stdio servers require a command".into());
                }
                if self
                    .url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty())
                {
                    return Err("stdio servers must not set a url".into());
                }
            }
            Transport::Http => {
                let url = self.url.as_deref().unwrap_or_default().trim();
                if !is_http_url(url) {
                    return Err(format!("http servers require an http(s) url, got: {url}"));
                }
                if self
                    .command
                    .as_deref()
                    .is_some_and(|c| !c.trim().is_empty())
                {
                    return Err("http servers must not set a command".into());
                }
            }
        }

        if !self.config.is_object() {
            return Err("config must be a JSON object".into());
        }
        for (name, value) in &self.headers {
            if name.trim().is_empty() {
                return Err("header names must not be empty".into());
            }
            if !value.is_string() {
                return Err(format!("header {name} must be a string"));
            }
        }
        Ok(())
    }
}

fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub enabled: bool,
    pub created_at: String,
}

impl McpTool {
    /// Name advertised to clients through `tools/list`.
    pub fn qualified_name(&self, server_name: &str) -> String {
        crate::protocol::qualify_tool_name(server_name, &self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpSession {
    pub id: String,
    pub server_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpInvocation {
    pub id: i64,
    pub server_id: String,
    pub tool_id: String,
    pub consumer_id: Option<String>,
    pub request_id: String,
    pub status: String,
    pub duration_ms: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

/// One ACL rule for a tool. Exactly one scope is set: a consumer, a consumer
/// group, or neither for the tool default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolAcl {
    pub id: String,
    pub tool_id: String,
    pub consumer_id: Option<String>,
    pub consumer_group_id: Option<String>,
    pub allowed: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApiKey {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub consumer_id: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// A newly minted API key. The plaintext is returned to the caller exactly
/// once and never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewApiKey {
    pub key: McpApiKey,
    pub plaintext: String,
}

/// Pure decision function for a tool invocation.
///
/// This is the legacy consumer-only view built from bare `allowed` flags and a
/// single "is a consumer known" bit. Scoped decisions (consumer, consumer
/// group, and tool default) use [`acl_allows_scoped`].
pub fn acl_decision(rows: impl IntoIterator<Item = bool>, consumer_known: bool) -> bool {
    let mut saw_row = false;
    for allowed in rows {
        saw_row = true;
        if allowed && consumer_known {
            return true;
        }
    }
    if saw_row { false } else { ACL_DEFAULT_ALLOW }
}

/// Scoped ACL decision for one tool.
///
/// Precedence, highest first:
/// 1. No rows for the tool -> the open-local default ([`ACL_DEFAULT_ALLOW`]).
/// 2. An explicit consumer row -> its `allowed` value.
/// 3. Group rows matching the caller's groups -> any explicit deny wins;
///    otherwise any allow grants.
/// 4. The tool default row (both scope ids null) -> its `allowed` value.
/// 5. Rows exist but none apply -> deny.
///
/// Anonymous callers (`consumer_id` is `None`) only match the default row.
pub fn acl_allows_scoped(
    rows: &[McpToolAcl],
    consumer_id: Option<&str>,
    group_ids: &[String],
) -> bool {
    if rows.is_empty() {
        return ACL_DEFAULT_ALLOW;
    }

    if let Some(consumer_id) = consumer_id {
        if let Some(row) = rows
            .iter()
            .find(|row| row.consumer_id.as_deref() == Some(consumer_id))
        {
            return row.allowed;
        }

        let mut saw_group = false;
        let mut group_allows = false;
        for row in rows {
            let Some(group_id) = row.consumer_group_id.as_deref() else {
                continue;
            };
            if !group_ids.iter().any(|id| id == group_id) {
                continue;
            }
            saw_group = true;
            if !row.allowed {
                return false;
            }
            group_allows = true;
        }
        if saw_group {
            return group_allows;
        }
    }

    rows.iter()
        .find(|row| row.consumer_id.is_none() && row.consumer_group_id.is_none())
        .is_some_and(|row| row.allowed)
}

/// Secrets live in references (`env:NAME`, `keyring:NAME`, or `inline:` is
/// rejected), never in the `headers_json` column.
pub fn validate_secret_reference(value: &str) -> Result<(), String> {
    if value.starts_with("inline:") {
        return Err("inline secrets are not allowed; use env: or keyring: references".into());
    }
    let scheme = value.split_once(':').map(|(scheme, _)| scheme);
    match scheme {
        Some("env") | Some("keyring") | Some("file") => Ok(()),
        _ => Err(format!(
            "header secret must be a reference such as env:NAME, got: {value}"
        )),
    }
}

/// Returns the reference string when the value is a `scheme:name` reference.
pub fn secret_reference(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|raw| validate_secret_reference(raw).is_ok())
}

pub fn empty_object() -> Value {
    json!({})
}

/// JSON passed to clients in `tools/list` items.
pub fn tool_descriptor(qualified: &str, description: &str, schema: &Value) -> Value {
    json!({
        "name": qualified,
        "description": description,
        "inputSchema": schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> NewMcpServer {
        NewMcpServer {
            name: name.into(),
            transport: Transport::Stdio,
            command: Some("node".into()),
            url: None,
            enabled: true,
            config: empty_object(),
            headers: Map::new(),
        }
    }

    #[test]
    fn validates_stdio_and_http_servers() {
        assert!(stdio("files").validate().is_ok());

        let mut bad = stdio("files");
        bad.command = None;
        assert!(bad.validate().is_err());

        let mut http = stdio("remote");
        http.transport = Transport::Http;
        http.command = None;
        http.url = Some("https://example.test/mcp".into());
        assert!(http.validate().is_ok());

        http.url = Some("ftp://example.test".into());
        assert!(http.validate().is_err());

        assert!(stdio("bad__name").validate().is_err());
    }

    #[test]
    fn acl_decision_follows_rows_or_default() {
        assert!(acl_decision(Vec::<bool>::new(), false));
        assert!(acl_decision([true], true));
        assert!(acl_decision([false, true], true));
        assert!(!acl_decision([false], true));
        assert!(!acl_decision([true], false));
    }

    fn acl(
        consumer_id: Option<&str>,
        consumer_group_id: Option<&str>,
        allowed: bool,
    ) -> McpToolAcl {
        McpToolAcl {
            id: format!("acl-{consumer_id:?}-{consumer_group_id:?}-{allowed}"),
            tool_id: "tool-1".into(),
            consumer_id: consumer_id.map(str::to_string),
            consumer_group_id: consumer_group_id.map(str::to_string),
            allowed,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn scoped_acl_without_rows_follows_open_default() {
        assert!(acl_allows_scoped(&[], Some("c1"), &["g1".into()]));
        assert!(acl_allows_scoped(&[], None, &[]));
    }

    #[test]
    fn scoped_acl_consumer_row_wins_over_group_and_default() {
        let rows = [
            acl(None, None, false),
            acl(None, Some("g1"), false),
            acl(Some("c1"), None, true),
        ];
        assert!(acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));

        let rows = [
            acl(None, None, true),
            acl(None, Some("g1"), true),
            acl(Some("c1"), None, false),
        ];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
    }

    #[test]
    fn scoped_acl_group_deny_overrides_group_allow() {
        let rows = [acl(None, Some("g1"), true), acl(None, Some("g2"), false)];
        let groups = ["g1".to_string(), "g2".to_string()];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &groups));

        // A group the caller is not in does not deny a matching allow.
        assert!(acl_allows_scoped(
            &[acl(None, Some("g1"), true), acl(None, Some("g2"), false)],
            Some("c1"),
            &["g1".into()]
        ));
    }

    #[test]
    fn scoped_acl_default_row_applies_when_no_scope_matches() {
        let rows = [acl(None, None, false), acl(None, Some("other"), true)];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));

        let rows = [acl(None, None, true)];
        assert!(acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
    }

    #[test]
    fn scoped_acl_anonymous_only_matches_default_row() {
        let rows = [acl(None, Some("g1"), true)];
        assert!(!acl_allows_scoped(&rows, None, &[]));
        // Even if group ids were supplied, an anonymous caller ignores them.
        assert!(!acl_allows_scoped(&rows, None, &["g1".into()]));

        let rows = [acl(None, None, true)];
        assert!(acl_allows_scoped(&rows, None, &[]));
    }

    #[test]
    fn scoped_acl_denies_when_no_row_applies() {
        let rows = [acl(Some("c2"), None, true), acl(None, Some("g2"), true)];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
        assert!(!acl_allows_scoped(&rows, None, &[]));
    }

    #[test]
    fn rejects_inline_secrets() {
        assert!(validate_secret_reference("env:TOKEN").is_ok());
        assert!(validate_secret_reference("keyring:svc").is_ok());
        assert!(validate_secret_reference("inline:abc").is_err());
        assert!(validate_secret_reference("plain-value").is_err());
    }

    #[test]
    fn tool_descriptor_uses_namespaced_name() {
        let descriptor = tool_descriptor("files__read", "Read", &json!({"type": "object"}));
        assert_eq!(descriptor["name"], "files__read");
        assert_eq!(descriptor["inputSchema"]["type"], "object");
    }
}
