//! MCP Gateway repositories: servers, tools, ACLs, keys, sessions, and
//! invocation records.

use octra_core::{Error, Result};
use octra_mcp_core::domain::McpTool;
use octra_mcp_core::{
    McpApiKey, McpInvocation, McpServer, McpSession, McpToolAcl, NewMcpServer, Transport,
};
use serde_json::{Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::Storage;

const SERVER_COLUMNS: &str = "id, name, transport, command, url, enabled, config_json, headers_json, \
     protocol_version, last_seen_at, created_at, updated_at";

const TOOL_COLUMNS: &str = "id, server_id, name, description, schema, enabled, created_at";

const SESSION_COLUMNS: &str = "id, server_id, status, created_at, updated_at";

const INVOCATION_COLUMNS: &str = "id, server_id, tool_id, consumer_id, request_id, status, \
     duration_ms, started_at, finished_at, error";

const ACL_COLUMNS: &str = "id, tool_id, consumer_id, allowed, created_at, updated_at";

const KEY_COLUMNS: &str = "id, name, key_prefix, consumer_id, enabled, expires_at, created_at";

/// A tool discovered on an upstream server, ready to be upserted.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
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

fn decode_json(value: Option<String>) -> Map<String, Value> {
    value
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn get_opt_i64(row: &SqliteRow, column: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(column).ok().flatten()
}

pub(crate) fn parse_transport(raw: &str) -> Transport {
    raw.parse().unwrap_or(Transport::Stdio)
}

impl Storage {
    fn mcp_pool(&self) -> &SqlitePool {
        self.pool()
    }

    // ----- servers -------------------------------------------------------

    pub async fn insert_mcp_server(&self, server: &NewMcpServer) -> Result<McpServer> {
        let id = uuid::Uuid::new_v4().to_string();
        let config = if server.config.as_object().is_some_and(|m| m.is_empty()) {
            "{}".to_string()
        } else {
            server.config.to_string()
        };
        let headers = Value::Object(server.headers.clone()).to_string();

        sqlx::query(
            "INSERT INTO mcp_servers (id, name, transport, command, url, enabled, config_json, headers_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&id)
        .bind(&server.name)
        .bind(server.transport.as_str())
        .bind(server.command.as_deref())
        .bind(server.url.as_deref())
        .bind(i64::from(server.enabled))
        .bind(config)
        .bind(headers)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot insert mcp server", err))?;

        self.get_mcp_server(&id)
            .await?
            .ok_or_else(|| Error::Storage("mcp server vanished after insert".to_string()))
    }

    /// Full update of a server definition. Returns `None` when the id is unknown.
    pub async fn update_mcp_server(
        &self,
        id: &str,
        server: &NewMcpServer,
    ) -> Result<Option<McpServer>> {
        let headers = Value::Object(server.headers.clone()).to_string();
        let result = sqlx::query(
            "UPDATE mcp_servers
                SET name = ?2, transport = ?3, command = ?4, url = ?5, enabled = ?6,
                    config_json = ?7, headers_json = ?8, protocol_version = NULL, last_seen_at = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(&server.name)
        .bind(server.transport.as_str())
        .bind(server.command.as_deref())
        .bind(server.url.as_deref())
        .bind(i64::from(server.enabled))
        .bind(server.config.to_string())
        .bind(headers)
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot update mcp server", err))?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_mcp_server(id).await
    }

    pub async fn get_mcp_server(&self, id: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp server", err))?;
        Ok(row.as_ref().map(decode_server))
    }

    pub async fn get_mcp_server_by_name(&self, name: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE name = ?1"
        ))
        .bind(name)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp server", err))?;
        Ok(row.as_ref().map(decode_server))
    }

    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers ORDER BY name ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp servers", err))?;
        Ok(rows.iter().map(decode_server).collect())
    }

    pub async fn delete_mcp_server(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp server", err))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_mcp_server_seen(&self, id: &str, protocol_version: &str) -> Result<()> {
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
    pub async fn replace_mcp_tools(
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
                "INSERT INTO mcp_tools (id, server_id, name, description, schema)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (server_id, name) DO UPDATE
                    SET description = excluded.description,
                        schema = excluded.schema",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(server_id)
            .bind(&tool.name)
            .bind(&tool.description)
            .bind(tool.schema.to_string())
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

        self.list_mcp_tools(server_id).await
    }

    pub async fn list_mcp_tools(&self, server_id: &str) -> Result<Vec<McpTool>> {
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
    pub async fn list_mcp_catalog(&self) -> Result<Vec<CatalogTool>> {
        let rows = sqlx::query(
            "SELECT t.id, t.server_id, t.name, t.description, t.schema, t.enabled, t.created_at, s.name AS server_name
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

    pub async fn get_mcp_tool(&self, id: &str) -> Result<Option<McpTool>> {
        let row = sqlx::query(&format!(
            "SELECT {TOOL_COLUMNS} FROM mcp_tools WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp tool", err))?;
        Ok(row.as_ref().map(decode_tool))
    }

    pub async fn list_all_mcp_tools(&self) -> Result<Vec<McpTool>> {
        let rows = sqlx::query(&format!(
            "SELECT {TOOL_COLUMNS} FROM mcp_tools ORDER BY name ASC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp tools", err))?;
        Ok(rows.iter().map(decode_tool).collect())
    }

    /// Toggles a tool and returns the new row, or `None` when the id is unknown.
    pub async fn set_mcp_tool_enabled(&self, id: &str, enabled: bool) -> Result<Option<McpTool>> {
        let result = sqlx::query("UPDATE mcp_tools SET enabled = ?2 WHERE id = ?1")
            .bind(id)
            .bind(i64::from(enabled))
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot update mcp tool", err))?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_mcp_tool(id).await
    }

    // ----- ACLs ----------------------------------------------------------

    /// Inserts or updates the (tool, consumer) rule and stores `allowed`.
    pub async fn upsert_mcp_acl(
        &self,
        tool_id: &str,
        consumer_id: &str,
        allowed: bool,
    ) -> Result<McpToolAcl> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO mcp_tool_acls (id, tool_id, consumer_id, allowed)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (tool_id, consumer_id) DO UPDATE
                SET allowed = excluded.allowed,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(&id)
        .bind(tool_id)
        .bind(consumer_id)
        .bind(i64::from(allowed))
        .execute(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot upsert mcp acl", err))?;

        sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM mcp_tool_acls WHERE tool_id = ?1 AND consumer_id = ?2"
        ))
        .bind(tool_id)
        .bind(consumer_id)
        .fetch_one(self.mcp_pool())
        .await
        .map(|row| decode_acl(&row))
        .map_err(|err| storage_error("cannot load mcp acl", err))
    }

    pub async fn get_mcp_acl(&self, id: &str) -> Result<Option<McpToolAcl>> {
        let row = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM mcp_tool_acls WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp acl", err))?;
        Ok(row.as_ref().map(decode_acl))
    }

    pub async fn list_mcp_acls(&self, tool_id: Option<&str>) -> Result<Vec<McpToolAcl>> {
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

    pub async fn delete_mcp_acl(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_tool_acls WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp acl", err))?;
        Ok(result.rows_affected() > 0)
    }

    /// The `allowed` flags recorded for one tool. Empty means no rules exist,
    /// which the ACL policy treats as the open-local default.
    pub async fn mcp_acl_flags(&self, tool_id: &str) -> Result<Vec<bool>> {
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
    pub async fn mcp_acl_allows(&self, tool_id: &str, consumer_id: &str) -> Result<bool> {
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

    // ----- consumers -----------------------------------------------------

    pub async fn ensure_consumer(&self, id: &str, name: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO consumers (id, name) VALUES (?1, ?2)")
            .bind(id)
            .bind(name)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot upsert consumer", err))?;
        Ok(())
    }

    pub async fn mcp_consumer_exists(&self, id: &str) -> Result<bool> {
        let found: Option<String> = sqlx::query_scalar("SELECT id FROM consumers WHERE id = ?1")
            .bind(id)
            .fetch_optional(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot load consumer", err))?;
        Ok(found.is_some())
    }

    // ----- API keys ------------------------------------------------------

    pub async fn insert_mcp_api_key(
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

        self.get_mcp_api_key(&id)
            .await?
            .ok_or_else(|| Error::Storage("mcp api key vanished after insert".to_string()))
    }

    pub async fn get_mcp_api_key(&self, id: &str) -> Result<Option<McpApiKey>> {
        let row = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM mcp_api_keys WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp api key", err))?;
        Ok(row.as_ref().map(decode_key))
    }

    pub async fn list_mcp_api_keys(&self) -> Result<Vec<McpApiKey>> {
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
    pub async fn find_active_mcp_api_key(&self, key_hash: &str) -> Result<Option<McpApiKey>> {
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

    pub async fn delete_mcp_api_key(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_api_keys WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp api key", err))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_mcp_api_key_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<McpApiKey>> {
        let result = sqlx::query("UPDATE mcp_api_keys SET enabled = ?2 WHERE id = ?1")
            .bind(id)
            .bind(i64::from(enabled))
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot update mcp api key", err))?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_mcp_api_key(id).await
    }

    // ----- sessions ------------------------------------------------------

    pub async fn insert_mcp_session(&self, id: &str, server_id: &str) -> Result<McpSession> {
        sqlx::query("INSERT INTO mcp_sessions (id, server_id) VALUES (?1, ?2)")
            .bind(id)
            .bind(server_id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot insert mcp session", err))?;
        self.get_mcp_session(id)
            .await?
            .ok_or_else(|| Error::Storage("mcp session vanished after insert".to_string()))
    }

    pub async fn get_mcp_session(&self, id: &str) -> Result<Option<McpSession>> {
        let row = sqlx::query(&format!(
            "SELECT {SESSION_COLUMNS} FROM mcp_sessions WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp session", err))?;
        Ok(row.as_ref().map(decode_session))
    }

    pub async fn list_mcp_sessions(&self) -> Result<Vec<McpSession>> {
        let rows = sqlx::query(&format!(
            "SELECT {SESSION_COLUMNS} FROM mcp_sessions ORDER BY created_at DESC"
        ))
        .fetch_all(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot list mcp sessions", err))?;
        Ok(rows.iter().map(decode_session).collect())
    }

    pub async fn terminate_mcp_session(&self, id: &str) -> Result<Option<McpSession>> {
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
        self.get_mcp_session(id).await
    }

    pub async fn delete_mcp_session(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_sessions WHERE id = ?1")
            .bind(id)
            .execute(self.mcp_pool())
            .await
            .map_err(|err| storage_error("cannot delete mcp session", err))?;
        Ok(result.rows_affected() > 0)
    }

    // ----- invocations ---------------------------------------------------

    /// Opens an invocation row and returns its id; the row is completed by
    /// [`Storage::finish_mcp_invocation`].
    pub async fn begin_mcp_invocation(
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

    pub async fn finish_mcp_invocation(
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
    pub async fn record_mcp_invocation(&self, record: &InvocationRecord<'_>) -> Result<i64> {
        let id = self
            .begin_mcp_invocation(
                record.server_id,
                record.tool_id,
                record.consumer_id,
                record.request_id,
            )
            .await?;
        self.finish_mcp_invocation(id, record.status, record.duration_ms, record.error)
            .await?;
        Ok(id)
    }

    pub async fn get_mcp_invocation(&self, id: i64) -> Result<Option<McpInvocation>> {
        let row = sqlx::query(&format!(
            "SELECT {INVOCATION_COLUMNS} FROM mcp_invocations WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.mcp_pool())
        .await
        .map_err(|err| storage_error("cannot load mcp invocation", err))?;
        Ok(row.as_ref().map(decode_invocation))
    }

    pub async fn list_mcp_invocations(
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
    pub async fn has_enabled_mcp_api_keys(&self) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_api_keys WHERE enabled = 1")
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

fn decode_tool(row: &SqliteRow) -> McpTool {
    let raw_schema: String = row.get("schema");
    let schema = serde_json::from_str(&raw_schema).unwrap_or(Value::Object(Map::new()));
    McpTool {
        id: row.get("id"),
        server_id: row.get("server_id"),
        name: row.get("name"),
        description: row.get("description"),
        schema,
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
    }
}

fn decode_session(row: &SqliteRow) -> McpSession {
    McpSession {
        id: row.get("id"),
        server_id: row.get("server_id"),
        status: row.get("status"),
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

    async fn storage() -> Storage {
        Storage::connect_in_memory().await.unwrap()
    }

    fn http_server(name: &str) -> NewMcpServer {
        NewMcpServer {
            name: name.into(),
            transport: Transport::Http,
            command: None,
            url: Some("https://example.test/mcp".into()),
            enabled: true,
            config: Value::Object(Map::new()),
            headers: Map::new(),
        }
    }

    fn discovered(name: &str) -> DiscoveredTool {
        DiscoveredTool {
            name: name.into(),
            description: format!("{name} tool"),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn server_crud_round_trips() {
        let storage = storage().await;
        let created = storage
            .insert_mcp_server(&http_server("remote"))
            .await
            .unwrap();
        assert_eq!(created.transport, Transport::Http);
        assert_eq!(created.config, Value::Object(Map::new()));

        let mut update = http_server("renamed");
        update.enabled = false;
        let updated = storage
            .update_mcp_server(&created.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert!(!updated.enabled);

        assert_eq!(storage.list_mcp_servers().await.unwrap().len(), 1);
        assert!(
            storage
                .get_mcp_server_by_name("renamed")
                .await
                .unwrap()
                .is_some()
        );

        assert!(
            storage
                .insert_mcp_server(&http_server("renamed"))
                .await
                .is_err()
        );
        assert!(storage.delete_mcp_server(&created.id).await.unwrap());
        assert!(storage.get_mcp_server(&created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tool_refresh_preserves_acl_rows() {
        let storage = storage().await;
        let server = storage
            .insert_mcp_server(&http_server("files"))
            .await
            .unwrap();
        let tools = storage
            .replace_mcp_tools(&server.id, &[discovered("read"), discovered("write")])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let read = tools.iter().find(|t| t.name == "read").unwrap();

        storage.ensure_consumer("c1", "consumer-one").await.unwrap();
        storage.upsert_mcp_acl(&read.id, "c1", false).await.unwrap();
        storage.set_mcp_tool_enabled(&read.id, false).await.unwrap();

        // Refresh keeps ids and enablement of surviving tools, so ACLs live on.
        let tools = storage
            .replace_mcp_tools(&server.id, &[discovered("read"), discovered("list")])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let read = tools.iter().find(|t| t.name == "read").unwrap();
        assert!(!read.enabled);
        assert_eq!(
            storage.list_mcp_acls(Some(&read.id)).await.unwrap().len(),
            1
        );

        let catalog = storage.list_mcp_catalog().await.unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(catalog.iter().all(|entry| entry.server_name == "files"));
        assert_eq!(catalog[0].qualified_name(), "files__list");
    }

    #[tokio::test]
    async fn acl_upsert_is_idempotent_per_pair() {
        let storage = storage().await;
        let server = storage
            .insert_mcp_server(&http_server("svc"))
            .await
            .unwrap();
        let tool = storage
            .replace_mcp_tools(&server.id, &[discovered("do")])
            .await
            .unwrap()
            .remove(0);
        storage.ensure_consumer("c1", "one").await.unwrap();

        let acl = storage.upsert_mcp_acl(&tool.id, "c1", true).await.unwrap();
        assert!(acl.allowed);
        let acl = storage.upsert_mcp_acl(&tool.id, "c1", false).await.unwrap();
        assert!(!acl.allowed);
        assert_eq!(storage.list_mcp_acls(None).await.unwrap().len(), 1);
        assert_eq!(storage.mcp_acl_flags(&tool.id).await.unwrap(), vec![false]);
        assert!(!storage.mcp_acl_allows(&tool.id, "c1").await.unwrap());
        assert!(storage.delete_mcp_acl(&acl.id).await.unwrap());
        assert!(storage.mcp_acl_flags(&tool.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_keys_resolve_only_when_enabled_and_unexpired() {
        let storage = storage().await;
        assert!(!storage.has_enabled_mcp_api_keys().await.unwrap());

        let key = storage
            .insert_mcp_api_key("agent", "octra_abcd", "hash-1", None, None)
            .await
            .unwrap();
        assert!(storage.has_enabled_mcp_api_keys().await.unwrap());
        assert_eq!(
            storage
                .find_active_mcp_api_key("hash-1")
                .await
                .unwrap()
                .unwrap()
                .id,
            key.id
        );

        storage
            .set_mcp_api_key_enabled(&key.id, false)
            .await
            .unwrap();
        assert!(
            storage
                .find_active_mcp_api_key("hash-1")
                .await
                .unwrap()
                .is_none()
        );

        let expired = storage
            .insert_mcp_api_key("old", "octra_old", "hash-2", None, Some("2000-01-01"))
            .await
            .unwrap();
        assert!(
            storage
                .find_active_mcp_api_key("hash-2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(storage.delete_mcp_api_key(&expired.id).await.unwrap());
        assert_eq!(storage.list_mcp_api_keys().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sessions_and_invocations_are_recorded() {
        let storage = storage().await;
        let server = storage
            .insert_mcp_server(&http_server("svc"))
            .await
            .unwrap();
        let tool = storage
            .replace_mcp_tools(&server.id, &[discovered("do")])
            .await
            .unwrap()
            .remove(0);

        let session = storage
            .insert_mcp_session("sess-1", &server.id)
            .await
            .unwrap();
        assert_eq!(session.status, octra_mcp_core::domain::session_status::OPEN);
        let terminated = storage
            .terminate_mcp_session("sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            terminated.status,
            octra_mcp_core::domain::session_status::TERMINATED
        );

        let id = storage
            .begin_mcp_invocation(&server.id, &tool.id, Some("c1"), "req-1")
            .await
            .unwrap();
        storage
            .finish_mcp_invocation(id, octra_mcp_core::domain::status::ERROR, 42, Some("boom"))
            .await
            .unwrap();
        let invocation = storage.get_mcp_invocation(id).await.unwrap().unwrap();
        assert_eq!(invocation.status, octra_mcp_core::domain::status::ERROR);
        assert_eq!(invocation.duration_ms, 42);
        assert_eq!(invocation.request_id, "req-1");
        assert_eq!(invocation.error.as_deref(), Some("boom"));
        assert!(invocation.finished_at.is_some());

        storage
            .record_mcp_invocation(&InvocationRecord {
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
            .list_mcp_invocations(10, Some(&server.id))
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].request_id, "req-2");
        assert!(
            storage
                .list_mcp_invocations(10, Some("other"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn consumers_are_inserted_once() {
        let storage = storage().await;
        storage.ensure_consumer("c1", "one").await.unwrap();
        storage.ensure_consumer("c1", "one").await.unwrap();
        assert!(storage.mcp_consumer_exists("c1").await.unwrap());
        assert!(!storage.mcp_consumer_exists("c2").await.unwrap());
        // A different name for the same consumer keeps the first row.
        storage.ensure_consumer("c1", "renamed").await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM consumers WHERE id = 'c1'")
            .fetch_one(storage.pool())
            .await
            .unwrap();
        assert_eq!(name, "one");
    }
}
