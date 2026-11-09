//! MCP Gateway repositories: servers, tools, ACLs, keys, sessions, and
//! invocation records.

use octra_core::{Error, Result};
use octra_mcp_core::domain::{
    MCP_TOOL_KIND_MCP, McpAuthStrategy, McpServerAcl, McpTool, NewMcpAuthStrategy,
};
use octra_mcp_core::{
    McpApiKey, McpInvocation, McpServer, McpSession, McpToolAcl, NewMcpServer, Transport,
};
use serde_json::{Map, Value, json};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

const SERVER_COLUMNS: &str = "id, name, transport, mode, sources_json, route_path, \
     acl_allow_when_json, acl_deny_when_json, auth_strategy_id, command, url, enabled, \
     config_json, headers_json, protocol_version, last_seen_at, created_at, updated_at";

const AUTH_STRATEGY_COLUMNS: &str =
    "id, name, kind, config_json, enabled, is_default, created_at, updated_at";

const TOOL_COLUMNS: &str = "id, server_id, name, description, schema, kind, http_method, \
     http_path, http_config_json, enabled, created_at";

const SESSION_COLUMNS: &str = "id, server_id, status, protocol_version, created_at, updated_at";

const INVOCATION_COLUMNS: &str = "id, server_id, tool_id, consumer_id, request_id, status, \
     duration_ms, started_at, finished_at, error";

const ACL_COLUMNS: &str =
    "id, tool_id, consumer_id, consumer_group_id, allowed, created_at, updated_at";

const SERVER_ACL_COLUMNS: &str =
    "id, server_id, consumer_id, consumer_group_id, allowed, created_at, updated_at";

const KEY_COLUMNS: &str = "id, name, key_prefix, consumer_id, enabled, expires_at, created_at";

/// A tool discovered on an upstream server or generated from an OpenAPI
/// document, ready to be upserted.
///
/// Regular MCP discoveries leave the HTTP fields at their defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub kind: String,
    pub http_method: Option<String>,
    pub http_path: Option<String>,
    pub http_config: Value,
}

impl Default for DiscoveredTool {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            schema: json!({"type": "object"}),
            kind: MCP_TOOL_KIND_MCP.to_string(),
            http_method: None,
            http_path: None,
            http_config: json!({}),
        }
    }
}

impl DiscoveredTool {
    /// A regular tool discovered from an MCP server.
    pub fn mcp(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            ..Self::default()
        }
    }

    /// An HTTP tool generated from an OpenAPI operation.
    pub fn http(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        method: impl Into<String>,
        path: impl Into<String>,
        config: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            kind: octra_mcp_core::domain::MCP_TOOL_KIND_HTTP.to_string(),
            http_method: Some(method.into()),
            http_path: Some(path.into()),
            http_config: config,
        }
    }
}

/// A completed invocation row written in one step.
#[derive(Debug, Clone, Copy)]
pub struct InvocationRecord<'a> {
    pub server_id: &'a str,
    pub tool_id: &'a str,
    pub consumer_id: Option<&'a str>,
    pub request_id: &'a str,
    pub status: &'a str,
    pub duration_ms: i64,
    pub error: Option<&'a str>,
}

/// A tool name advertised by the gateway plus its persisted identity.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTool {
    pub tool: McpTool,
    pub server_name: String,
}

impl CatalogTool {
    pub fn qualified_name(&self) -> String {
        self.tool.qualified_name(&self.server_name)
    }
}

fn storage_error(context: &str, err: sqlx::Error) -> Error {
    if matches!(err, sqlx::Error::Database(_)) && is_unique_violation(&err) {
        return Error::Storage(format!("{context}: duplicate value"));
    }
    Error::Storage(format!("{context}: {err}"))
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "1555" || code == "2067")
}

/// Clears the single default flag inside an open transaction.
async fn clear_default_auth_strategy(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<()> {
    sqlx::query(
        "UPDATE mcp_auth_strategies
            SET is_default = 0,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE is_default = 1",
    )
    .execute(&mut **tx)
    .await
    .map_err(|err| storage_error("cannot clear default mcp auth strategy", err))?;
    Ok(())
}

fn decode_json(value: Option<String>) -> Map<String, Value> {
    value
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// Decodes a JSON array of strings. Malformed or non-array values degrade to
/// an empty list, matching the "no sources configured" behaviour.
fn decode_string_list(value: Option<String>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

fn encode_string_list(values: &[String]) -> String {
    Value::Array(values.iter().cloned().map(Value::String).collect()).to_string()
}

fn get_opt_i64(row: &SqliteRow, column: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(column).ok().flatten()
}

pub(crate) fn parse_transport(raw: &str) -> Transport {
    raw.parse().unwrap_or(Transport::Stdio)
}

#[derive(Clone)]
pub struct McpStore {
    pool: SqlitePool,
}

impl McpStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn mcp_pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ----- auth strategies ----------------------------------------------

    /// Creates a strategy. Setting `is_default` clears the previous default in
    /// the same transaction.
    pub async fn insert_auth_strategy(
        &self,
        strategy: &NewMcpAuthStrategy,
    ) -> Result<McpAuthStrategy> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut tx = self
            .mcp_pool()
            .begin()
            .await
            .map_err(|err| storage_error("cannot begin auth strategy insert", err))?;

        if strategy.is_default {
            clear_default_auth_strategy(&mut tx).await?;
        }

        sqlx::query(
            "INSERT INTO mcp_auth_strategies
                 (id, name, kind, config_json, enabled, is_default)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(&strategy.name)
        .bind(&strategy.kind)
        .bind(strategy.config.to_string())
        .bind(i64::from(strategy.enabled))
        .bind(i64::from(strategy.is_default))
        .execute(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot insert mcp auth strategy", err))?;

        tx.commit()
            .await
            .map_err(|err| storage_error("cannot commit auth strategy insert", err))?;

        self.get_auth_strategy(&id)
            .await?
            .ok_or_else(|| Error::Storage("mcp auth strategy vanished after insert".to_string()))
    }

    /// Full update of a strategy. Returns `None` when the id is unknown.
    pub async fn update_auth_strategy(
        &self,
        id: &str,
        strategy: &NewMcpAuthStrategy,
    ) -> Result<Option<McpAuthStrategy>> {
        let mut tx = self
            .mcp_pool()
            .begin()
            .await
            .map_err(|err| storage_error("cannot begin auth strategy update", err))?;

        if strategy.is_default {
            clear_default_auth_strategy(&mut tx).await?;
        }

        let result = sqlx::query(
            "UPDATE mcp_auth_strategies
                SET name = ?2,
                    kind = ?3,
                    config_json = ?4,
                    enabled = ?5,
                    is_default = ?6,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(&strategy.name)
        .bind(&strategy.kind)
        .bind(strategy.config.to_string())
        .bind(i64::from(strategy.enabled))
        .bind(i64::from(strategy.is_default))
        .execute(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot update mcp auth strategy", err))?;

        if result.rows_affected() == 0 {
            tx.rollback()
                .await
                .map_err(|err| storage_error("cannot roll back auth strategy update", err))?;
            return Ok(None);
        }

        tx.commit()
            .await
            .map_err(|err| storage_error("cannot commit auth strategy update", err))?;
        self.get_auth_strategy(id).await
    }

    pub async fn get_auth_strategy(&self, id: &str) -> Result<Option<McpAuthStrategy>> {
        let row = sqlx::query(&format!(
            "SELECT {AUTH_STRATEGY_COLUMNS} FROM mcp_auth_strategies WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp auth strategy", err))?;
        Ok(row.as_ref().map(decode_auth_strategy))
    }

    pub async fn get_auth_strategy_by_name(&self, name: &str) -> Result<Option<McpAuthStrategy>> {
        let row = sqlx::query(&format!(
            "SELECT {AUTH_STRATEGY_COLUMNS} FROM mcp_auth_strategies WHERE name = ?1"
        ))
        .bind(name)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp auth strategy", err))?;
        Ok(row.as_ref().map(decode_auth_strategy))
    }

    pub async fn list_auth_strategies(&self) -> Result<Vec<McpAuthStrategy>> {
        let rows = sqlx::query(&format!(
            "SELECT {AUTH_STRATEGY_COLUMNS} FROM mcp_auth_strategies ORDER BY name ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp auth strategies", err))?;
        Ok(rows.iter().map(decode_auth_strategy).collect())
    }

    /// The enabled default strategy, if one is configured.
    pub async fn get_default_auth_strategy(&self) -> Result<Option<McpAuthStrategy>> {
        let row = sqlx::query(&format!(
            "SELECT {AUTH_STRATEGY_COLUMNS} FROM mcp_auth_strategies
              WHERE is_default = 1 AND enabled = 1
              LIMIT 1"
        ))
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load default mcp auth strategy", err))?;
        Ok(row.as_ref().map(decode_auth_strategy))
    }

    /// Deletes a strategy. Server references become NULL through the foreign
    /// key, so those servers follow the default strategy again.
    pub async fn delete_auth_strategy(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_auth_strategies WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp auth strategy", err))?;
        Ok(result.rows_affected() > 0)
    }

    // ----- servers -------------------------------------------------------

    pub async fn insert_server(&self, server: &NewMcpServer) -> Result<McpServer> {
        let id = uuid::Uuid::new_v4().to_string();
        let config = if server.config.as_object().is_some_and(|m| m.is_empty()) {
            "{}".to_string()
        } else {
            server.config.to_string()
        };
        let headers = Value::Object(server.headers.clone()).to_string();
        let sources = encode_string_list(&server.sources);
        let allow_when = encode_string_list(&server.acl_allow_when);
        let deny_when = encode_string_list(&server.acl_deny_when);

        sqlx::query(
            "INSERT INTO mcp_servers
                 (id, name, transport, mode, sources_json, route_path,
                  acl_allow_when_json, acl_deny_when_json, auth_strategy_id, command, url,
                  enabled, config_json, headers_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(&id)
        .bind(&server.name)
        .bind(server.transport.as_str())
        .bind(&server.mode)
        .bind(sources)
        .bind(server.route_path.as_deref())
        .bind(allow_when)
        .bind(deny_when)
        .bind(server.auth_strategy_id.as_deref())
        .bind(server.command.as_deref())
        .bind(server.url.as_deref())
        .bind(i64::from(server.enabled))
        .bind(config)
        .bind(headers)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot insert mcp server", err))?;

        self.get_server(&id)
            .await?
            .ok_or_else(|| Error::Storage("mcp server vanished after insert".to_string()))
    }

    /// Full update of a server definition. Returns `None` when the id is unknown.
    pub async fn update_server(
        &self,
        id: &str,
        server: &NewMcpServer,
    ) -> Result<Option<McpServer>> {
        let headers = Value::Object(server.headers.clone()).to_string();
        let sources = encode_string_list(&server.sources);
        let allow_when = encode_string_list(&server.acl_allow_when);
        let deny_when = encode_string_list(&server.acl_deny_when);
        let result = sqlx::query(
            "UPDATE mcp_servers
                SET name = ?2, transport = ?3, mode = ?4, sources_json = ?5, command = ?6,
                    url = ?7, enabled = ?8, config_json = ?9, headers_json = ?10,
                    route_path = ?11, acl_allow_when_json = ?12, acl_deny_when_json = ?13,
                    auth_strategy_id = ?14,
                    protocol_version = NULL, last_seen_at = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(&server.name)
        .bind(server.transport.as_str())
        .bind(&server.mode)
        .bind(sources)
        .bind(server.command.as_deref())
        .bind(server.url.as_deref())
        .bind(i64::from(server.enabled))
        .bind(server.config.to_string())
        .bind(headers)
        .bind(server.route_path.as_deref())
        .bind(allow_when)
        .bind(deny_when)
        .bind(server.auth_strategy_id.as_deref())
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot update mcp server", err))?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_server(id).await
    }

    pub async fn get_server(&self, id: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp server", err))?;
        Ok(row.as_ref().map(decode_server))
    }

    pub async fn get_server_by_name(&self, name: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE name = ?1"
        ))
        .bind(name)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp server", err))?;
        Ok(row.as_ref().map(decode_server))
    }

    pub async fn list_servers(&self) -> Result<Vec<McpServer>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers ORDER BY name ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp servers", err))?;
        Ok(rows.iter().map(decode_server).collect())
    }

    pub async fn delete_server(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp server", err))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_server_seen(&self, id: &str, protocol_version: &str) -> Result<()> {
        sqlx::query(
            "UPDATE mcp_servers
                SET protocol_version = ?2,
                    last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(protocol_version)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot update mcp server status", err))?;
        Ok(())
    }

    // ----- tools ---------------------------------------------------------

    /// Replaces the tool set of a server with what discovery returned. Tools
    /// keep their id (and therefore their ACL rows and enable flag) when the
    /// name is unchanged; tools the server no longer exposes are dropped.
    pub async fn replace_tools(
        &self,
        server_id: &str,
        discovered: &[DiscoveredTool],
    ) -> Result<Vec<McpTool>> {
        let mut tx = self
            .mcp_pool()
            .begin()
            .await
            .map_err(|err| storage_error("cannot begin tool refresh", err))?;

        for tool in discovered {
            sqlx::query(
                "INSERT INTO mcp_tools
                     (id, server_id, name, description, schema, kind, http_method,
                      http_path, http_config_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (server_id, name) DO UPDATE
                    SET description = excluded.description,
                        schema = excluded.schema,
                        kind = excluded.kind,
                        http_method = excluded.http_method,
                        http_path = excluded.http_path,
                        http_config_json = excluded.http_config_json",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(server_id)
            .bind(&tool.name)
            .bind(&tool.description)
            .bind(tool.schema.to_string())
            .bind(&tool.kind)
            .bind(tool.http_method.as_deref())
            .bind(tool.http_path.as_deref())
            .bind(tool.http_config.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|err| storage_error("cannot upsert mcp tool", err))?;
        }

        let names: Vec<String> = discovered.iter().map(|tool| tool.name.clone()).collect();
        sqlx::query("DELETE FROM mcp_tools WHERE server_id = ?1 AND name NOT IN (SELECT value FROM json_each(?2))")
            .bind(server_id)
            .bind(Value::Array(names.into_iter().map(Value::String).collect()).to_string())
            .execute(&mut *tx)
            .await
            .map_err(|err| storage_error("cannot prune mcp tools", err))?;

        tx.commit()
            .await
            .map_err(|err| storage_error("cannot commit tool refresh", err))?;

        self.list_tools(server_id).await
    }

    pub async fn list_tools(&self, server_id: &str) -> Result<Vec<McpTool>> {
        let rows = sqlx::query(&format!(
            "SELECT {TOOL_COLUMNS} FROM mcp_tools WHERE server_id = ?1 ORDER BY name ASC"
        ))
        .bind(server_id)
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp tools", err))?;
        Ok(rows.iter().map(decode_tool).collect())
    }

    /// Every tool of every server, with the owning server name, ordered by
    /// server then tool for a stable catalog.
    pub async fn list_catalog(&self) -> Result<Vec<CatalogTool>> {
        let rows = sqlx::query(
            "SELECT t.id, t.server_id, t.name, t.description, t.schema, t.kind,
                    t.http_method, t.http_path, t.http_config_json, t.enabled,
                    t.created_at, s.name AS server_name
               FROM mcp_tools t
               JOIN mcp_servers s ON s.id = t.server_id
              ORDER BY s.name ASC, t.name ASC",
        )
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp catalog", err))?;
        Ok(rows
            .iter()
            .map(|row| CatalogTool {
                tool: decode_tool(row),
                server_name: row.get("server_name"),
            })
            .collect())
    }

    pub async fn get_tool(&self, id: &str) -> Result<Option<McpTool>> {
        let row = sqlx::query(&format!(
            "SELECT {TOOL_COLUMNS} FROM mcp_tools WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp tool", err))?;
        Ok(row.as_ref().map(decode_tool))
    }

    pub async fn list_all_tools(&self) -> Result<Vec<McpTool>> {
        let rows = sqlx::query(&format!(
            "SELECT {TOOL_COLUMNS} FROM mcp_tools ORDER BY name ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp tools", err))?;
        Ok(rows.iter().map(decode_tool).collect())
    }

    /// Toggles a tool and returns the new row, or `None` when the id is unknown.
    pub async fn set_tool_enabled(&self, id: &str, enabled: bool) -> Result<Option<McpTool>> {
        let result = sqlx::query("UPDATE mcp_tools SET enabled = ?2 WHERE id = ?1")
            .bind(id)
            .bind(i64::from(enabled))
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot update mcp tool", err))?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_tool(id).await
    }

    // ----- ACLs ----------------------------------------------------------

    /// Inserts or updates the rule for one scope and stores `allowed`.
    ///
    /// The scope is a consumer, a consumer group, or the tool default when both
    /// ids are `None`. Pass exactly one scope; passing both ids is an error.
    /// The update runs before the insert because SQLite partial unique indexes
    /// cannot be used as `ON CONFLICT` targets.
    pub async fn upsert_acl(
        &self,
        tool_id: &str,
        consumer_id: Option<&str>,
        consumer_group_id: Option<&str>,
        allowed: bool,
    ) -> Result<McpToolAcl> {
        if consumer_id.is_some() && consumer_group_id.is_some() {
            return Err(Error::Storage(
                "mcp acl scope must target at most one of consumer_id and consumer_group_id"
                    .to_string(),
            ));
        }

        // Matches exactly one scope: the requested ids, with the other scope
        // column required to be null.
        let scope = "tool_id = ?1 \
             AND ((?2 IS NULL AND consumer_id IS NULL) OR consumer_id = ?2) \
             AND ((?3 IS NULL AND consumer_group_id IS NULL) OR consumer_group_id = ?3)";

        let mut tx = self
            .mcp_pool()
            .begin()
            .await
            .map_err(|err| storage_error("cannot begin mcp acl upsert", err))?;

        let updated = sqlx::query(&format!(
            "UPDATE mcp_tool_acls
                SET allowed = ?4,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE {scope}"
        ))
        .bind(tool_id)
        .bind(consumer_id)
        .bind(consumer_group_id)
        .bind(i64::from(allowed))
        .execute(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot update mcp acl", err))?;

        if updated.rows_affected() == 0 {
            sqlx::query(
                "INSERT INTO mcp_tool_acls (id, tool_id, consumer_id, consumer_group_id, allowed)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(tool_id)
            .bind(consumer_id)
            .bind(consumer_group_id)
            .bind(i64::from(allowed))
            .execute(&mut *tx)
            .await
            .map_err(|err| storage_error("cannot insert mcp acl", err))?;
        }

        let row = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM mcp_tool_acls WHERE {scope}"
        ))
        .bind(tool_id)
        .bind(consumer_id)
        .bind(consumer_group_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot load mcp acl", err))?;

        tx.commit()
            .await
            .map_err(|err| storage_error("cannot commit mcp acl upsert", err))?;

        Ok(decode_acl(&row))
    }

    pub async fn get_acl(&self, id: &str) -> Result<Option<McpToolAcl>> {
        let row = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM mcp_tool_acls WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp acl", err))?;
        Ok(row.as_ref().map(decode_acl))
    }

    pub async fn list_acls(&self, tool_id: Option<&str>) -> Result<Vec<McpToolAcl>> {
        let rows = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM mcp_tool_acls
              WHERE (?1 IS NULL OR tool_id = ?1)
              ORDER BY created_at ASC"
        ))
        .bind(tool_id)
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp acls", err))?;
        Ok(rows.iter().map(decode_acl).collect())
    }

    pub async fn delete_acl(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_tool_acls WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp acl", err))?;
        Ok(result.rows_affected() > 0)
    }

    /// The `allowed` flags recorded for one tool. Empty means no rules exist,
    /// which the ACL policy treats as the open-local default.
    pub async fn acl_flags(&self, tool_id: &str) -> Result<Vec<bool>> {
        let rows = sqlx::query("SELECT allowed FROM mcp_tool_acls WHERE tool_id = ?1")
            .bind(tool_id)
            .fetch_all(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot load mcp acl flags", err))?;
        Ok(rows
            .iter()
            .map(|row| row.get::<i64, _>("allowed") != 0)
            .collect())
    }

    /// True when a rule exists that allows this consumer on this tool.
    pub async fn acl_allows(&self, tool_id: &str, consumer_id: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT allowed FROM mcp_tool_acls WHERE tool_id = ?1 AND consumer_id = ?2",
        )
        .bind(tool_id)
        .bind(consumer_id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp acl", err))?;
        Ok(row.is_some_and(|row| row.get::<i64, _>("allowed") != 0))
    }

    // ----- server ACLs ---------------------------------------------------

    /// Inserts or updates the server-default rule for one scope.
    ///
    /// The scope is a consumer, a consumer group, or the server default when
    /// both ids are `None`. Rules at this level apply to every tool of the
    /// server that has no ACL rows of its own.
    pub async fn upsert_server_acl(
        &self,
        server_id: &str,
        consumer_id: Option<&str>,
        consumer_group_id: Option<&str>,
        allowed: bool,
    ) -> Result<McpServerAcl> {
        if consumer_id.is_some() && consumer_group_id.is_some() {
            return Err(Error::Storage(
                "mcp server acl scope must target at most one of consumer_id and consumer_group_id"
                    .to_string(),
            ));
        }

        // Matches exactly one scope: the requested ids, with the other scope
        // column required to be null.
        let scope = "server_id = ?1 \
             AND ((?2 IS NULL AND consumer_id IS NULL) OR consumer_id = ?2) \
             AND ((?3 IS NULL AND consumer_group_id IS NULL) OR consumer_group_id = ?3)";

        let mut tx = self
            .mcp_pool()
            .begin()
            .await
            .map_err(|err| storage_error("cannot begin mcp server acl upsert", err))?;

        let updated = sqlx::query(&format!(
            "UPDATE mcp_server_acls
                SET allowed = ?4,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE {scope}"
        ))
        .bind(server_id)
        .bind(consumer_id)
        .bind(consumer_group_id)
        .bind(i64::from(allowed))
        .execute(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot update mcp server acl", err))?;

        if updated.rows_affected() == 0 {
            sqlx::query(
                "INSERT INTO mcp_server_acls (id, server_id, consumer_id, consumer_group_id, allowed)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(server_id)
            .bind(consumer_id)
            .bind(consumer_group_id)
            .bind(i64::from(allowed))
            .execute(&mut *tx)
            .await
            .map_err(|err| storage_error("cannot insert mcp server acl", err))?;
        }

        let row = sqlx::query(&format!(
            "SELECT {SERVER_ACL_COLUMNS} FROM mcp_server_acls WHERE {scope}"
        ))
        .bind(server_id)
        .bind(consumer_id)
        .bind(consumer_group_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| storage_error("cannot load mcp server acl", err))?;

        tx.commit()
            .await
            .map_err(|err| storage_error("cannot commit mcp server acl upsert", err))?;

        Ok(decode_server_acl(&row))
    }

    pub async fn get_server_acl(&self, id: &str) -> Result<Option<McpServerAcl>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVER_ACL_COLUMNS} FROM mcp_server_acls WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp server acl", err))?;
        Ok(row.as_ref().map(decode_server_acl))
    }

    pub async fn list_server_acls(&self, server_id: &str) -> Result<Vec<McpServerAcl>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERVER_ACL_COLUMNS} FROM mcp_server_acls
              WHERE server_id = ?1
              ORDER BY created_at ASC"
        ))
        .bind(server_id)
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp server acls", err))?;
        Ok(rows.iter().map(decode_server_acl).collect())
    }

    pub async fn delete_server_acl(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_server_acls WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp server acl", err))?;
        Ok(result.rows_affected() > 0)
    }

    // ----- consumers -----------------------------------------------------

    pub async fn ensure_consumer(&self, id: &str, name: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO consumers (id, username) VALUES (?1, ?2)")
            .bind(id)
            .bind(name)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot upsert consumer", err))?;
        Ok(())
    }

    pub async fn consumer_exists(&self, id: &str) -> Result<bool> {
        let found: Option<String> = sqlx::query_scalar("SELECT id FROM consumers WHERE id = ?1")
            .bind(id)
            .fetch_optional(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot load consumer", err))?;
        Ok(found.is_some())
    }

    // ----- API keys ------------------------------------------------------

    pub async fn insert_api_key(
        &self,
        name: &str,
        key_prefix: &str,
        key_hash: &str,
        consumer_id: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<McpApiKey> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO mcp_api_keys (id, name, key_prefix, key_hash, consumer_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(name)
        .bind(key_prefix)
        .bind(key_hash)
        .bind(consumer_id)
        .bind(expires_at)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot insert mcp api key", err))?;

        self.get_api_key(&id)
            .await?
            .ok_or_else(|| Error::Storage("mcp api key vanished after insert".to_string()))
    }

    pub async fn get_api_key(&self, id: &str) -> Result<Option<McpApiKey>> {
        let row = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM mcp_api_keys WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp api key", err))?;
        Ok(row.as_ref().map(decode_key))
    }

    pub async fn list_api_keys(&self) -> Result<Vec<McpApiKey>> {
        let rows = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM mcp_api_keys ORDER BY created_at ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp api keys", err))?;
        Ok(rows.iter().map(decode_key).collect())
    }

    /// Resolves an enabled, unexpired key by its hash. Expiry dates are
    /// `YYYY-MM-DD` compared against UTC today.
    pub async fn find_active_api_key(&self, key_hash: &str) -> Result<Option<McpApiKey>> {
        let row = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM mcp_api_keys
              WHERE key_hash = ?1
                AND enabled = 1
                AND (expires_at IS NULL OR expires_at >= strftime('%Y-%m-%d', 'now'))"
        ))
        .bind(key_hash)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot resolve mcp api key", err))?;
        Ok(row.as_ref().map(decode_key))
    }

    pub async fn delete_api_key(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_api_keys WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp api key", err))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_api_key_enabled(&self, id: &str, enabled: bool) -> Result<Option<McpApiKey>> {
        let result = sqlx::query("UPDATE mcp_api_keys SET enabled = ?2 WHERE id = ?1")
            .bind(id)
            .bind(i64::from(enabled))
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot update mcp api key", err))?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_api_key(id).await
    }

    // ----- sessions ------------------------------------------------------

    pub async fn insert_session(
        &self,
        id: &str,
        server_id: &str,
        protocol_version: &str,
    ) -> Result<McpSession> {
        sqlx::query(
            "INSERT INTO mcp_sessions (id, server_id, protocol_version) VALUES (?1, ?2, ?3)",
        )
        .bind(id)
        .bind(server_id)
        .bind(protocol_version)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot insert mcp session", err))?;
        self.get_session(id)
            .await?
            .ok_or_else(|| Error::Storage("mcp session vanished after insert".to_string()))
    }

    pub async fn get_session(&self, id: &str) -> Result<Option<McpSession>> {
        let row = sqlx::query(&format!(
            "SELECT {SESSION_COLUMNS} FROM mcp_sessions WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp session", err))?;
        Ok(row.as_ref().map(decode_session))
    }

    pub async fn list_sessions(&self) -> Result<Vec<McpSession>> {
        let rows = sqlx::query(&format!(
            "SELECT {SESSION_COLUMNS} FROM mcp_sessions ORDER BY created_at DESC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp sessions", err))?;
        Ok(rows.iter().map(decode_session).collect())
    }

    /// Deletes sessions older than an RFC3339 cutoff. Called opportunistically
    /// when a new session is created so expired rows do not accumulate.
    pub async fn delete_sessions_created_before(&self, cutoff: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM mcp_sessions WHERE created_at < ?1")
            .bind(cutoff)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot prune mcp sessions", err))?;
        Ok(result.rows_affected())
    }

    pub async fn terminate_session(&self, id: &str) -> Result<Option<McpSession>> {
        let result = sqlx::query(
            "UPDATE mcp_sessions
                SET status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(octra_mcp_core::domain::session_status::TERMINATED)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot terminate mcp session", err))?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_session(id).await
    }

    pub async fn delete_session(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_sessions WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp session", err))?;
        Ok(result.rows_affected() > 0)
    }

    // ----- invocations ---------------------------------------------------

    /// Opens an invocation row and returns its id; the row is completed by
    /// [`Storage::finish_invocation`].
    pub async fn begin_invocation(
        &self,
        server_id: &str,
        tool_id: &str,
        consumer_id: Option<&str>,
        request_id: &str,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO mcp_invocations (server_id, tool_id, consumer_id, request_id, status)
             VALUES (?1, ?2, ?3, ?4, 'pending')",
        )
        .bind(server_id)
        .bind(tool_id)
        .bind(consumer_id)
        .bind(request_id)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot begin mcp invocation", err))?;
        Ok(result.last_insert_rowid())
    }

    pub async fn finish_invocation(
        &self,
        id: i64,
        status: &str,
        duration_ms: i64,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE mcp_invocations
                SET status = ?2,
                    duration_ms = ?3,
                    error = ?4,
                    finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(status)
        .bind(duration_ms)
        .bind(error)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot finish mcp invocation", err))?;
        Ok(())
    }

    /// Records a one-shot invocation that never reached an upstream.
    pub async fn record_invocation(&self, record: &InvocationRecord<'_>) -> Result<i64> {
        let id = self
            .begin_invocation(
                record.server_id,
                record.tool_id,
                record.consumer_id,
                record.request_id,
            )
            .await?;
        self.finish_invocation(id, record.status, record.duration_ms, record.error)
            .await?;
        Ok(id)
    }

    pub async fn get_invocation(&self, id: i64) -> Result<Option<McpInvocation>> {
        let row = sqlx::query(&format!(
            "SELECT {INVOCATION_COLUMNS} FROM mcp_invocations WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp invocation", err))?;
        Ok(row.as_ref().map(decode_invocation))
    }

    pub async fn list_invocations(
        &self,
        limit: i64,
        server_id: Option<&str>,
    ) -> Result<Vec<McpInvocation>> {
        let rows = sqlx::query(&format!(
            "SELECT {INVOCATION_COLUMNS} FROM mcp_invocations
              WHERE (?2 IS NULL OR server_id = ?2)
              ORDER BY id DESC
              LIMIT ?1"
        ))
        .bind(limit.clamp(1, 1000))
        .bind(server_id)
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp invocations", err))?;
        Ok(rows.iter().map(decode_invocation).collect())
    }

    /// True when any enabled API key exists; gates whether auth is required.
    pub async fn has_enabled_api_keys(&self) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_api_keys WHERE enabled = 1")
            .fetch_one(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot count mcp api keys", err))?;
        Ok(count > 0)
    }

    /// Whether the installation has ever been configured with an API key.
    /// Disabled/expired keys still count, so revocation can fail closed rather
    /// than silently reopening the gateway.
    pub async fn has_any_api_keys(&self) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_api_keys")
            .fetch_one(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot count mcp api keys", err))?;
        Ok(count > 0)
    }
}

fn decode_server(row: &SqliteRow) -> McpServer {
    McpServer {
        id: row.get("id"),
        name: row.get("name"),
        transport: parse_transport(&row.get::<String, _>("transport")),
        mode: row.get("mode"),
        sources: decode_string_list(row.get("sources_json")),
        route_path: row.get("route_path"),
        acl_allow_when: decode_string_list(row.get("acl_allow_when_json")),
        acl_deny_when: decode_string_list(row.get("acl_deny_when_json")),
        auth_strategy_id: row.get("auth_strategy_id"),
        command: row.get("command"),
        url: row.get("url"),
        enabled: row.get::<i64, _>("enabled") != 0,
        config: decode_json(row.get("config_json")).into(),
        headers: decode_json(row.get("headers_json")),
        protocol_version: row.get("protocol_version"),
        last_seen_at: row.get("last_seen_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn decode_auth_strategy(row: &SqliteRow) -> McpAuthStrategy {
    McpAuthStrategy {
        id: row.get("id"),
        name: row.get("name"),
        kind: row.get("kind"),
        config: decode_json(row.get("config_json")).into(),
        enabled: row.get::<i64, _>("enabled") != 0,
        is_default: row.get::<i64, _>("is_default") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn decode_tool(row: &SqliteRow) -> McpTool {
    let raw_schema: String = row.get("schema");
    let schema = serde_json::from_str(&raw_schema).unwrap_or(Value::Object(Map::new()));
    let kind = row
        .try_get::<String, _>("kind")
        .unwrap_or_else(|_| MCP_TOOL_KIND_MCP.to_string());
    let http_config = row
        .try_get::<Option<String>, _>("http_config_json")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    McpTool {
        id: row.get("id"),
        server_id: row.get("server_id"),
        name: row.get("name"),
        description: row.get("description"),
        schema,
        kind,
        http_method: row.try_get("http_method").ok().flatten(),
        http_path: row.try_get("http_path").ok().flatten(),
        http_config,
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
    }
}

fn decode_session(row: &SqliteRow) -> McpSession {
    McpSession {
        id: row.get("id"),
        server_id: row.get("server_id"),
        status: row.get("status"),
        protocol_version: row.get("protocol_version"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn decode_invocation(row: &SqliteRow) -> McpInvocation {
    McpInvocation {
        id: row.get("id"),
        server_id: row.get("server_id"),
        tool_id: row.get("tool_id"),
        consumer_id: row.get("consumer_id"),
        request_id: row.get("request_id"),
        status: row.get("status"),
        duration_ms: get_opt_i64(row, "duration_ms").unwrap_or(0),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        error: row.get("error"),
    }
}

fn decode_acl(row: &SqliteRow) -> McpToolAcl {
    McpToolAcl {
        id: row.get("id"),
        tool_id: row.get("tool_id"),
        consumer_id: row.get("consumer_id"),
        consumer_group_id: row.get("consumer_group_id"),
        allowed: row.get::<i64, _>("allowed") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn decode_server_acl(row: &SqliteRow) -> McpServerAcl {
    McpServerAcl {
        id: row.get("id"),
        server_id: row.get("server_id"),
        consumer_id: row.get("consumer_id"),
        consumer_group_id: row.get("consumer_group_id"),
        allowed: row.get::<i64, _>("allowed") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn decode_key(row: &SqliteRow) -> McpApiKey {
    McpApiKey {
        id: row.get("id"),
        name: row.get("name"),
        key_prefix: row.get("key_prefix"),
        consumer_id: row.get("consumer_id"),
        enabled: row.get::<i64, _>("enabled") != 0,
        expires_at: row.get("expires_at"),
        created_at: row.get("created_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    async fn storage() -> Storage {
        Storage::connect_in_memory().await.unwrap()
    }

    fn http_server(name: &str) -> NewMcpServer {
        NewMcpServer {
            name: name.into(),
            transport: Transport::Http,
            mode: octra_mcp_core::domain::default_server_mode(),
            sources: Vec::new(),
            route_path: None,
            acl_allow_when: Vec::new(),
            acl_deny_when: Vec::new(),
            auth_strategy_id: None,
            command: None,
            url: Some("https://example.test/mcp".into()),
            enabled: true,
            config: Value::Object(Map::new()),
            headers: Map::new(),
        }
    }

    fn discovered(name: &str) -> DiscoveredTool {
        DiscoveredTool::mcp(
            name,
            format!("{name} tool"),
            serde_json::json!({"type": "object"}),
        )
    }

    fn discovered_http(name: &str) -> DiscoveredTool {
        DiscoveredTool::http(
            name,
            format!("{name} http tool"),
            serde_json::json!({
                "type": "object",
                "properties": {"path_id": {"type": "string"}},
            }),
            "get",
            "/pets/{id}",
            serde_json::json!({
                "base_url": "https://api.example.test",
                "parameters": [{"arg": "path_id", "name": "id", "in": "path"}],
            }),
        )
    }

    fn oidc_strategy(name: &str, is_default: bool) -> NewMcpAuthStrategy {
        NewMcpAuthStrategy {
            name: name.into(),
            kind: "oidc".into(),
            config: serde_json::json!({
                "issuer": "https://issuer.example.test",
                "jwks_uri": "https://issuer.example.test/jwks",
            }),
            enabled: true,
            is_default,
        }
    }

    #[tokio::test]
    async fn auth_strategy_crud_keeps_a_single_default() {
        let storage = storage().await;

        let first = storage
            .mcp
            .insert_auth_strategy(&oidc_strategy("first", true))
            .await
            .unwrap();
        assert!(first.is_default);
        assert_eq!(first.kind, "oidc");
        assert_eq!(first.config["issuer"], "https://issuer.example.test");
        assert_eq!(
            storage
                .mcp
                .get_default_auth_strategy()
                .await
                .unwrap()
                .unwrap()
                .id,
            first.id
        );

        let second = storage
            .mcp
            .insert_auth_strategy(&oidc_strategy("second", false))
            .await
            .unwrap();
        assert!(!second.is_default);
        assert_eq!(
            storage
                .mcp
                .get_default_auth_strategy()
                .await
                .unwrap()
                .unwrap()
                .id,
            first.id
        );

        // Promoting the second clears the first in the same transaction.
        let promoted = storage
            .mcp
            .update_auth_strategy(&second.id, &oidc_strategy("second", true))
            .await
            .unwrap()
            .unwrap();
        assert!(promoted.is_default);
        assert!(
            !storage
                .mcp
                .get_auth_strategy(&first.id)
                .await
                .unwrap()
                .unwrap()
                .is_default
        );
        assert_eq!(
            storage
                .mcp
                .list_auth_strategies()
                .await
                .unwrap()
                .iter()
                .filter(|strategy| strategy.is_default)
                .count(),
            1
        );

        let by_name = storage
            .mcp
            .get_auth_strategy_by_name("first")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name.id, first.id);

        assert!(storage.mcp.delete_auth_strategy(&second.id).await.unwrap());
        assert!(
            storage
                .mcp
                .get_default_auth_strategy()
                .await
                .unwrap()
                .is_none()
        );
        assert!(!storage.mcp.delete_auth_strategy(&second.id).await.unwrap());
    }

    #[tokio::test]
    async fn server_auth_strategy_reference_round_trips_and_clears_on_delete() {
        let storage = storage().await;
        let strategy = storage
            .mcp
            .insert_auth_strategy(&oidc_strategy("sso", false))
            .await
            .unwrap();

        let mut server = http_server("remote");
        server.auth_strategy_id = Some(strategy.id.clone());
        let created = storage.mcp.insert_server(&server).await.unwrap();
        assert_eq!(
            created.auth_strategy_id.as_deref(),
            Some(strategy.id.as_str())
        );

        let loaded = storage.mcp.get_server(&created.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.auth_strategy_id.as_deref(),
            Some(strategy.id.as_str())
        );

        // An update can clear the reference.
        let update = http_server("remote");
        let updated = storage
            .mcp
            .update_server(&created.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert!(updated.auth_strategy_id.is_none());

        // And re-attach it before deletion.
        let mut update = http_server("remote");
        update.auth_strategy_id = Some(strategy.id.clone());
        storage
            .mcp
            .update_server(&created.id, &update)
            .await
            .unwrap()
            .unwrap();

        assert!(
            storage
                .mcp
                .delete_auth_strategy(&strategy.id)
                .await
                .unwrap()
        );
        let loaded = storage.mcp.get_server(&created.id).await.unwrap().unwrap();
        assert!(
            loaded.auth_strategy_id.is_none(),
            "deleting a strategy clears server references"
        );
    }

    #[tokio::test]
    async fn server_crud_round_trips() {
        let storage = storage().await;
        let created = storage
            .mcp
            .insert_server(&http_server("remote"))
            .await
            .unwrap();
        assert_eq!(created.transport, Transport::Http);
        assert_eq!(created.config, Value::Object(Map::new()));

        let mut update = http_server("renamed");
        update.enabled = false;
        let updated = storage
            .mcp
            .update_server(&created.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert!(!updated.enabled);

        assert_eq!(storage.mcp.list_servers().await.unwrap().len(), 1);
        assert!(
            storage
                .mcp
                .get_server_by_name("renamed")
                .await
                .unwrap()
                .is_some()
        );

        assert!(
            storage
                .mcp
                .insert_server(&http_server("renamed"))
                .await
                .is_err()
        );
        assert!(storage.mcp.delete_server(&created.id).await.unwrap());
        assert!(storage.mcp.get_server(&created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn server_mode_and_sources_round_trip() {
        let storage = storage().await;

        // New rows default to the legacy passthrough listener with no sources.
        let created = storage
            .mcp
            .insert_server(&http_server("plain"))
            .await
            .unwrap();
        assert_eq!(
            created.mode,
            octra_mcp_core::domain::MCP_SERVER_MODE_PASSTHROUGH_LISTENER
        );
        assert!(created.sources.is_empty());

        let mut listener = http_server("aggregate");
        listener.mode = octra_mcp_core::domain::MCP_SERVER_MODE_LISTENER.into();
        listener.sources = vec!["plain".into(), "remote".into()];
        let listener = storage.mcp.insert_server(&listener).await.unwrap();
        assert_eq!(
            listener.mode,
            octra_mcp_core::domain::MCP_SERVER_MODE_LISTENER
        );
        assert_eq!(listener.sources, vec!["plain", "remote"]);

        let loaded = storage.mcp.get_server(&listener.id).await.unwrap().unwrap();
        assert_eq!(loaded.mode, "listener");
        assert_eq!(loaded.sources, vec!["plain", "remote"]);

        // Updates replace mode and sources wholesale.
        let mut update = http_server("aggregate");
        update.mode = octra_mcp_core::domain::MCP_SERVER_MODE_UPSTREAM_SERVER.into();
        let updated = storage
            .mcp
            .update_server(&listener.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.mode, "upstream-server");
        assert!(updated.sources.is_empty());

        // A malformed sources column degrades to an empty list, never an error.
        sqlx::query("UPDATE mcp_servers SET sources_json = 'not-json' WHERE id = ?1")
            .bind(&listener.id)
            .execute(storage.pool())
            .await
            .unwrap();
        let loaded = storage.mcp.get_server(&listener.id).await.unwrap().unwrap();
        assert!(loaded.sources.is_empty());
    }

    #[tokio::test]
    async fn server_route_path_and_cel_expressions_round_trip() {
        let storage = storage().await;

        // Unset means the gateway computes `/mcp/<name>`; the column stays null.
        let created = storage
            .mcp
            .insert_server(&http_server("plain"))
            .await
            .unwrap();
        assert!(created.route_path.is_none());
        assert!(created.acl_allow_when.is_empty());
        assert!(created.acl_deny_when.is_empty());

        let mut custom = http_server("custom");
        custom.route_path = Some("/mcp/team/custom".into());
        custom.acl_allow_when = vec!["consumer_group.names.contains(\"team\")".into()];
        custom.acl_deny_when = vec!["consumer.id == \"blocked\"".into()];
        let custom = storage.mcp.insert_server(&custom).await.unwrap();
        assert_eq!(custom.route_path.as_deref(), Some("/mcp/team/custom"));
        assert_eq!(custom.acl_allow_when.len(), 1);
        assert_eq!(custom.acl_deny_when.len(), 1);

        // Explicit route paths are unique when not null.
        let mut duplicate = http_server("duplicate");
        duplicate.route_path = Some("/mcp/team/custom".into());
        let error = storage.mcp.insert_server(&duplicate).await.unwrap_err();
        assert!(error.to_string().contains("duplicate value"), "{error}");

        // Updates replace the expressions wholesale and can clear the path.
        let mut update = http_server("custom");
        update.acl_deny_when = vec!["request.method == \"DELETE\"".into()];
        let updated = storage
            .mcp
            .update_server(&custom.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert!(updated.route_path.is_none());
        assert!(updated.acl_allow_when.is_empty());
        assert_eq!(updated.acl_deny_when.len(), 1);

        // A malformed expression column degrades to an empty list.
        sqlx::query("UPDATE mcp_servers SET acl_allow_when_json = 'not-json' WHERE id = ?1")
            .bind(&custom.id)
            .execute(storage.pool())
            .await
            .unwrap();
        let loaded = storage.mcp.get_server(&custom.id).await.unwrap().unwrap();
        assert!(loaded.acl_allow_when.is_empty());
    }

    #[tokio::test]
    async fn server_acls_upsert_per_scope_and_override_independently() {
        let storage = storage().await;
        let server = storage
            .mcp
            .insert_server(&http_server("svc"))
            .await
            .unwrap();
        let other = storage
            .mcp
            .insert_server(&http_server("other"))
            .await
            .unwrap();
        storage.mcp.ensure_consumer("c1", "one").await.unwrap();
        let group = storage
            .api
            .create_consumer_group(&octra_api_core::CreateConsumerGroup {
                name: "team".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let acl = storage
            .mcp
            .upsert_server_acl(&server.id, Some("c1"), None, true)
            .await
            .unwrap();
        assert_eq!(acl.server_id, server.id);
        assert!(acl.allowed);

        // The same scope updates in place.
        let acl = storage
            .mcp
            .upsert_server_acl(&server.id, Some("c1"), None, false)
            .await
            .unwrap();
        assert!(!acl.allowed);
        assert_eq!(
            storage
                .mcp
                .list_server_acls(&server.id)
                .await
                .unwrap()
                .len(),
            1
        );

        // Group and default scopes are independent rows.
        storage
            .mcp
            .upsert_server_acl(&server.id, None, Some(&group.id), true)
            .await
            .unwrap();
        let default_acl = storage
            .mcp
            .upsert_server_acl(&server.id, None, None, false)
            .await
            .unwrap();
        assert!(default_acl.consumer_id.is_none());
        assert!(default_acl.consumer_group_id.is_none());
        assert!(!default_acl.allowed);
        assert_eq!(
            storage
                .mcp
                .list_server_acls(&server.id)
                .await
                .unwrap()
                .len(),
            3
        );

        // Another server keeps its own empty rule set.
        assert!(
            storage
                .mcp
                .list_server_acls(&other.id)
                .await
                .unwrap()
                .is_empty()
        );

        // Both scopes at once is rejected before touching SQL.
        assert!(
            storage
                .mcp
                .upsert_server_acl(&server.id, Some("c1"), Some(&group.id), true)
                .await
                .is_err()
        );
        assert_eq!(
            storage
                .mcp
                .list_server_acls(&server.id)
                .await
                .unwrap()
                .len(),
            3
        );

        let loaded = storage.mcp.get_server_acl(&acl.id).await.unwrap().unwrap();
        assert!(!loaded.allowed);
        assert!(storage.mcp.delete_server_acl(&acl.id).await.unwrap());
        assert!(storage.mcp.get_server_acl(&acl.id).await.unwrap().is_none());
        assert_eq!(
            storage
                .mcp
                .list_server_acls(&server.id)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn tool_refresh_preserves_acl_rows() {
        let storage = storage().await;
        let server = storage
            .mcp
            .insert_server(&http_server("files"))
            .await
            .unwrap();
        let tools = storage
            .mcp
            .replace_tools(&server.id, &[discovered("read"), discovered("write")])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let read = tools.iter().find(|t| t.name == "read").unwrap();

        storage
            .mcp
            .ensure_consumer("c1", "consumer-one")
            .await
            .unwrap();
        storage
            .mcp
            .upsert_acl(&read.id, Some("c1"), None, false)
            .await
            .unwrap();
        storage.mcp.set_tool_enabled(&read.id, false).await.unwrap();

        // Refresh keeps ids and enablement of surviving tools, so ACLs live on.
        let tools = storage
            .mcp
            .replace_tools(&server.id, &[discovered("read"), discovered("list")])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let read = tools.iter().find(|t| t.name == "read").unwrap();
        assert!(!read.enabled);
        assert_eq!(
            storage.mcp.list_acls(Some(&read.id)).await.unwrap().len(),
            1
        );

        let catalog = storage.mcp.list_catalog().await.unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(catalog.iter().all(|entry| entry.server_name == "files"));
        assert_eq!(catalog[0].qualified_name(), "files__list");
    }

    #[tokio::test]
    async fn http_tool_columns_round_trip_and_refresh_replaces_them() {
        let storage = storage().await;
        let server = storage
            .mcp
            .insert_server(&http_server("converted"))
            .await
            .unwrap();

        let tools = storage
            .mcp
            .replace_tools(
                &server.id,
                &[discovered("plain"), discovered_http("get_pet")],
            )
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let plain = tools.iter().find(|tool| tool.name == "plain").unwrap();
        assert_eq!(plain.kind, octra_mcp_core::domain::MCP_TOOL_KIND_MCP);
        assert!(plain.http_method.is_none());
        assert_eq!(plain.http_config, serde_json::json!({}));

        let http = tools.iter().find(|tool| tool.name == "get_pet").unwrap();
        assert_eq!(http.kind, octra_mcp_core::domain::MCP_TOOL_KIND_HTTP);
        assert_eq!(http.http_method.as_deref(), Some("get"));
        assert_eq!(http.http_path.as_deref(), Some("/pets/{id}"));
        assert_eq!(
            http.http_config["parameters"][0]["arg"],
            serde_json::json!("path_id")
        );

        // Refresh rewrites the generated set and keeps ids for surviving names.
        let tools = storage
            .mcp
            .replace_tools(&server.id, &[discovered_http("list_pets")])
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "list_pets");
        assert!(tools[0].is_http());
    }

    #[tokio::test]
    async fn acl_upsert_is_idempotent_per_scope() {
        let storage = storage().await;
        let server = storage
            .mcp
            .insert_server(&http_server("svc"))
            .await
            .unwrap();
        let tool = storage
            .mcp
            .replace_tools(&server.id, &[discovered("do")])
            .await
            .unwrap()
            .remove(0);
        storage.mcp.ensure_consumer("c1", "one").await.unwrap();
        let group = storage
            .api
            .create_consumer_group(&octra_api_core::CreateConsumerGroup {
                name: "team".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let acl = storage
            .mcp
            .upsert_acl(&tool.id, Some("c1"), None, true)
            .await
            .unwrap();
        assert!(acl.allowed);
        assert_eq!(acl.consumer_id.as_deref(), Some("c1"));
        assert!(acl.consumer_group_id.is_none());

        // A second upsert on the same scope updates the row in place.
        let acl = storage
            .mcp
            .upsert_acl(&tool.id, Some("c1"), None, false)
            .await
            .unwrap();
        assert!(!acl.allowed);
        assert_eq!(storage.mcp.list_acls(None).await.unwrap().len(), 1);
        assert_eq!(storage.mcp.acl_flags(&tool.id).await.unwrap(), vec![false]);
        assert!(!storage.mcp.acl_allows(&tool.id, "c1").await.unwrap());

        // Group scope round-trips and is independent of the consumer row.
        let group_acl = storage
            .mcp
            .upsert_acl(&tool.id, None, Some(&group.id), true)
            .await
            .unwrap();
        assert_eq!(
            group_acl.consumer_group_id.as_deref(),
            Some(group.id.as_str())
        );
        assert!(group_acl.consumer_id.is_none());
        assert!(group_acl.allowed);

        // The tool default has both ids null and updates in place too.
        let default_acl = storage
            .mcp
            .upsert_acl(&tool.id, None, None, false)
            .await
            .unwrap();
        assert!(default_acl.consumer_id.is_none());
        assert!(default_acl.consumer_group_id.is_none());
        assert!(!default_acl.allowed);
        let default_acl = storage
            .mcp
            .upsert_acl(&tool.id, None, None, true)
            .await
            .unwrap();
        assert!(default_acl.allowed);
        assert_eq!(storage.mcp.list_acls(None).await.unwrap().len(), 3);
        assert_eq!(
            storage.mcp.list_acls(Some(&tool.id)).await.unwrap().len(),
            3
        );

        // Both ids at once is rejected before touching SQL.
        assert!(
            storage
                .mcp
                .upsert_acl(&tool.id, Some("c1"), Some(&group.id), true)
                .await
                .is_err()
        );
        assert_eq!(storage.mcp.list_acls(None).await.unwrap().len(), 3);

        storage.mcp.delete_acl(&acl.id).await.unwrap();
        assert_eq!(storage.mcp.list_acls(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_keys_resolve_only_when_enabled_and_unexpired() {
        let storage = storage().await;
        assert!(!storage.mcp.has_enabled_api_keys().await.unwrap());

        let key = storage
            .mcp
            .insert_api_key("agent", "octra_abcd", "hash-1", None, None)
            .await
            .unwrap();
        assert!(storage.mcp.has_enabled_api_keys().await.unwrap());
        assert_eq!(
            storage
                .mcp
                .find_active_api_key("hash-1")
                .await
                .unwrap()
                .unwrap()
                .id,
            key.id
        );

        storage
            .mcp
            .set_api_key_enabled(&key.id, false)
            .await
            .unwrap();
        assert!(
            storage
                .mcp
                .find_active_api_key("hash-1")
                .await
                .unwrap()
                .is_none()
        );

        let expired = storage
            .mcp
            .insert_api_key("old", "octra_old", "hash-2", None, Some("2000-01-01"))
            .await
            .unwrap();
        assert!(
            storage
                .mcp
                .find_active_api_key("hash-2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(storage.mcp.delete_api_key(&expired.id).await.unwrap());
        assert_eq!(storage.mcp.list_api_keys().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sessions_and_invocations_are_recorded() {
        let storage = storage().await;
        let server = storage
            .mcp
            .insert_server(&http_server("svc"))
            .await
            .unwrap();
        let tool = storage
            .mcp
            .replace_tools(&server.id, &[discovered("do")])
            .await
            .unwrap()
            .remove(0);

        let session = storage
            .mcp
            .insert_session("sess-1", &server.id, "2025-06-18")
            .await
            .unwrap();
        assert_eq!(session.status, octra_mcp_core::domain::session_status::OPEN);
        let terminated = storage
            .mcp
            .terminate_session("sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            terminated.status,
            octra_mcp_core::domain::session_status::TERMINATED
        );

        let id = storage
            .mcp
            .begin_invocation(&server.id, &tool.id, Some("c1"), "req-1")
            .await
            .unwrap();
        storage
            .mcp
            .finish_invocation(id, octra_mcp_core::domain::status::ERROR, 42, Some("boom"))
            .await
            .unwrap();
        let invocation = storage.mcp.get_invocation(id).await.unwrap().unwrap();
        assert_eq!(invocation.status, octra_mcp_core::domain::status::ERROR);
        assert_eq!(invocation.duration_ms, 42);
        assert_eq!(invocation.request_id, "req-1");
        assert_eq!(invocation.error.as_deref(), Some("boom"));
        assert!(invocation.finished_at.is_some());

        storage
            .mcp
            .record_invocation(&InvocationRecord {
                server_id: &server.id,
                tool_id: &tool.id,
                consumer_id: None,
                request_id: "req-2",
                status: octra_mcp_core::domain::status::DENIED,
                duration_ms: 0,
                error: None,
            })
            .await
            .unwrap();
        let listed = storage
            .mcp
            .list_invocations(10, Some(&server.id))
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].request_id, "req-2");
        assert!(
            storage
                .mcp
                .list_invocations(10, Some("other"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn consumers_are_inserted_once() {
        let storage = storage().await;
        storage.mcp.ensure_consumer("c1", "one").await.unwrap();
        storage.mcp.ensure_consumer("c1", "one").await.unwrap();
        assert!(storage.mcp.consumer_exists("c1").await.unwrap());
        assert!(!storage.mcp.consumer_exists("c2").await.unwrap());
        // A different name for the same consumer keeps the first row.
        storage.mcp.ensure_consumer("c1", "renamed").await.unwrap();
        let name: String = sqlx::query_scalar("SELECT username FROM consumers WHERE id = 'c1'")
            .fetch_one(storage.pool())
            .await
            .unwrap();
        assert_eq!(name, "one");
    }
}
