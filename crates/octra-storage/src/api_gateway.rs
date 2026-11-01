//! API gateway repositories: services, routes, upstreams, targets, consumers,
//! and hashed API credentials.

use std::collections::BTreeMap;

use octra_api_core::{
    ApiCredential, Consumer, CreateApiCredential, CreateConsumer, CreatePolicy, CreateRoute,
    CreateService, CreateTarget, CreateUpstream, LoadBalanceAlgorithm, Policy, Route, Service,
    Target, UpdateApiCredential, UpdateConsumer, UpdatePolicy, UpdateRoute, UpdateService,
    UpdateTarget, UpdateUpstream,
};
use octra_core::{Error, Result};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};

use crate::Storage;

const SERVICE_COLUMNS: &str = "id, name, protocol, description, enabled, connect_timeout_ms, \
     request_timeout_ms, retries, require_auth, created_at, updated_at";
const ROUTE_COLUMNS: &str = "id, service_id, host, path_prefix, methods, headers, protocol, \
     priority, strip_prefix, enabled, created_at, updated_at";
const UPSTREAM_COLUMNS: &str = "id, service_id, name, algorithm, created_at, updated_at";
const TARGET_COLUMNS: &str =
    "id, upstream_id, url, weight, healthy, enabled, created_at, updated_at";
const CONSUMER_COLUMNS: &str = "id, name, kind, created_at, updated_at";
const CREDENTIAL_COLUMNS: &str = "id, consumer_id, name, kind, key_prefix, key_hash, enabled, expires_at, last_used_at, created_at";
const POLICY_COLUMNS: &str = "id, name, scope, consumer_id, document, created_at, updated_at";

const SQLITE_CONSTRAINT_UNIQUE: &str = "2067";
const SQLITE_CONSTRAINT_PRIMARYKEY: &str = "1555";
const SQLITE_CONSTRAINT_FOREIGNKEY: &str = "787";

/// One coherent view of everything the API data plane needs at runtime.
#[derive(Debug, Clone, Default)]
pub struct ApiRuntimeData {
    pub services: Vec<Service>,
    pub routes: Vec<Route>,
    pub upstreams: Vec<UpstreamRow>,
    pub targets: Vec<Target>,
    pub credentials: Vec<ApiCredential>,
    pub policies: Vec<Policy>,
    pub counts: ApiRuntimeCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiRuntimeCounts {
    pub services: i64,
    pub routes: i64,
    pub upstreams: i64,
    pub targets: i64,
    pub consumers: i64,
    pub credentials: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRow {
    pub id: String,
    pub service_id: String,
    pub algorithm: LoadBalanceAlgorithm,
}

fn now_expr() -> &'static str {
    "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
}

/// Maps driver errors to typed storage errors with stable markers so the
/// control plane can translate them into HTTP status codes.
pub fn map_db_error(error: sqlx::Error, entity: &str) -> Error {
    match error {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => match db.code().as_deref() {
            Some(SQLITE_CONSTRAINT_PRIMARYKEY | SQLITE_CONSTRAINT_UNIQUE) => {
                Error::Storage(format!("conflict: {entity} already exists"))
            }
            _ => Error::Storage(format!("conflict: {entity} violates a uniqueness rule")),
        },
        sqlx::Error::Database(ref db)
            if db.code().as_deref() == Some(SQLITE_CONSTRAINT_FOREIGNKEY) =>
        {
            Error::Storage(format!("not_found: {entity} references a missing parent"))
        }
        sqlx::Error::RowNotFound => Error::Storage(format!("not_found: {entity} not found")),
        other => Error::Storage(format!("cannot persist {entity}: {other}")),
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn parse_timestamp(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|err| Error::Storage(format!("invalid timestamp '{value}': {err}")))
}

fn opt_timestamp(value: Option<String>) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    value.as_deref().map(parse_timestamp).transpose()
}

fn map_service(row: &SqliteRow) -> Result<Service> {
    Ok(Service {
        id: decode(row.try_get("id"), "service")?,
        name: decode(row.try_get("name"), "service")?,
        protocol: decode(row.try_get("protocol"), "service")?,
        description: decode(row.try_get("description"), "service")?,
        enabled: decode(row.try_get::<i64, _>("enabled"), "service")? != 0,
        connect_timeout_ms: decode(row.try_get("connect_timeout_ms"), "service")?,
        request_timeout_ms: decode(row.try_get("request_timeout_ms"), "service")?,
        retries: decode(row.try_get("retries"), "service")?,
        require_auth: decode(row.try_get::<i64, _>("require_auth"), "service")? != 0,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "service")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "service")?)?,
    })
}

fn map_route(row: &SqliteRow) -> Result<Route> {
    let methods_json: String = decode(row.try_get("methods"), "route")?;
    let headers_json: String = decode(row.try_get("headers"), "route")?;
    let methods: Vec<String> = serde_json_map(&methods_json, "methods")?;
    let headers: BTreeMap<String, String> = serde_json_map(&headers_json, "headers")?;

    Ok(Route {
        id: decode(row.try_get("id"), "route")?,
        service_id: decode(row.try_get("service_id"), "route")?,
        host: decode(row.try_get("host"), "route")?,
        path_prefix: decode(row.try_get("path_prefix"), "route")?,
        methods,
        headers,
        protocol: decode(row.try_get("protocol"), "route")?,
        priority: decode(row.try_get("priority"), "route")?,
        strip_prefix: decode(row.try_get::<i64, _>("strip_prefix"), "route")? != 0,
        enabled: decode(row.try_get::<i64, _>("enabled"), "route")? != 0,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "route")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "route")?)?,
    })
}

fn serde_json_map<T: serde::de::DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(raw)
        .map_err(|err| Error::Storage(format!("invalid {field} JSON '{raw}': {err}")))
}

fn map_upstream(row: &SqliteRow) -> Result<octra_api_core::Upstream> {
    let algorithm: String = decode(row.try_get("algorithm"), "upstream")?;
    Ok(octra_api_core::Upstream {
        id: decode(row.try_get("id"), "upstream")?,
        service_id: decode(row.try_get("service_id"), "upstream")?,
        name: decode(row.try_get("name"), "upstream")?,
        algorithm: LoadBalanceAlgorithm::parse(&algorithm).ok_or_else(|| {
            Error::Storage(format!("unknown load balance algorithm '{algorithm}'"))
        })?,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "upstream")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "upstream")?)?,
    })
}

fn map_target(row: &SqliteRow) -> Result<Target> {
    Ok(Target {
        id: decode(row.try_get("id"), "target")?,
        upstream_id: decode(row.try_get("upstream_id"), "target")?,
        url: decode(row.try_get("url"), "target")?,
        weight: u32::try_from(decode::<i64>(row.try_get("weight"), "target")?)
            .map_err(|_| Error::Storage("target weight out of range".into()))?,
        healthy: decode(row.try_get::<i64, _>("healthy"), "target")? != 0,
        enabled: decode(row.try_get::<i64, _>("enabled"), "target")? != 0,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "target")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "target")?)?,
    })
}

fn map_consumer(row: &SqliteRow) -> Result<Consumer> {
    Ok(Consumer {
        id: decode(row.try_get("id"), "consumer")?,
        name: decode(row.try_get("name"), "consumer")?,
        kind: decode(row.try_get("kind"), "consumer")?,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "consumer")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "consumer")?)?,
    })
}

fn map_policy(row: &SqliteRow) -> Result<Policy> {
    let document: String = decode(row.try_get("document"), "policy")?;
    Ok(Policy {
        id: decode(row.try_get("id"), "policy")?,
        name: decode(row.try_get("name"), "policy")?,
        scope: decode(row.try_get("scope"), "policy")?,
        consumer_id: row
            .try_get("consumer_id")
            .map_err(|e| Error::Storage(format!("policy: {e}")))?,
        document: serde_json::from_str(&document)
            .map_err(|e| Error::Storage(format!("invalid policy document: {e}")))?,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "policy")?)?,
        updated_at: parse_timestamp(&decode::<String>(row.try_get("updated_at"), "policy")?)?,
    })
}

fn map_credential(row: &SqliteRow) -> Result<ApiCredential> {
    Ok(ApiCredential {
        id: decode(row.try_get("id"), "credential")?,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        name: decode(row.try_get("name"), "credential")?,
        kind: decode(row.try_get("kind"), "credential")?,
        key_prefix: decode(row.try_get("key_prefix"), "credential")?,
        key_hash: decode(row.try_get("key_hash"), "credential")?,
        enabled: decode(row.try_get::<i64, _>("enabled"), "credential")? != 0,
        expires_at: opt_timestamp(decode(row.try_get("expires_at"), "credential")?)?,
        last_used_at: opt_timestamp(decode(row.try_get("last_used_at"), "credential")?)?,
        created_at: parse_timestamp(&decode::<String>(row.try_get("created_at"), "credential")?)?,
    })
}

fn affected(rows: u64, entity: &str) -> Result<()> {
    if rows == 0 {
        Err(Error::NotFound(format!("{entity} not found")))
    } else {
        Ok(())
    }
}

/// Unwraps a row decode result, mapping failures to storage errors.
fn decode<T>(value: sqlx::error::Result<T>, entity: &str) -> Result<T> {
    value.map_err(|e| map_db_error(e, entity))
}

impl Storage {
    // Services

    pub async fn list_services(&self) -> Result<Vec<Service>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERVICE_COLUMNS} FROM services ORDER BY name ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "services"))?;

        rows.iter().map(map_service).collect()
    }

    pub async fn get_service(&self, id: &str) -> Result<Option<Service>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVICE_COLUMNS} FROM services WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        row.as_ref().map(map_service).transpose()
    }

    pub async fn create_service(&self, input: &CreateService) -> Result<Service> {
        let id = input.id.clone().unwrap_or_else(|| new_id("svc"));
        let protocol = input.protocol.clone().unwrap_or_else(|| "http".into());
        let description = input.description.clone().unwrap_or_default();

        sqlx::query(&format!(
            "INSERT INTO services (id, name, protocol, description, enabled, connect_timeout_ms, \
             request_timeout_ms, retries, require_auth, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&protocol)
        .bind(&description)
        .bind(input.enabled.unwrap_or(true))
        .bind(input.connect_timeout_ms.unwrap_or(10_000))
        .bind(input.request_timeout_ms.unwrap_or(60_000))
        .bind(input.retries.unwrap_or(0))
        .bind(input.require_auth.unwrap_or(true))
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        self.get_service(&id)
            .await?
            .ok_or_else(|| Error::Storage("service disappeared after insert".into()))
    }

    pub async fn update_service(&self, id: &str, input: &UpdateService) -> Result<Service> {
        let result = sqlx::query(&format!(
            "UPDATE services SET \
                name = COALESCE(?, name), \
                protocol = COALESCE(?, protocol), \
                description = COALESCE(?, description), \
                enabled = COALESCE(?, enabled), \
                connect_timeout_ms = COALESCE(?, connect_timeout_ms), \
                request_timeout_ms = COALESCE(?, request_timeout_ms), \
                retries = COALESCE(?, retries), \
                require_auth = COALESCE(?, require_auth), \
                updated_at = {now} \
             WHERE id = ?",
            now = now_expr()
        ))
        .bind(&input.name)
        .bind(&input.protocol)
        .bind(&input.description)
        .bind(input.enabled)
        .bind(input.connect_timeout_ms)
        .bind(input.request_timeout_ms)
        .bind(input.retries)
        .bind(input.require_auth)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        affected(result.rows_affected(), "service")?;
        self.get_service(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: service not found".into()))
    }

    pub async fn delete_service(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM services WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "service"))?;
        affected(result.rows_affected(), "service")
    }

    // Routes

    pub async fn list_routes(&self, service_id: Option<&str>) -> Result<Vec<Route>> {
        let rows = sqlx::query(&format!(
            "SELECT {ROUTE_COLUMNS} FROM routes WHERE (?1 IS NULL OR service_id = ?1) \
             ORDER BY priority DESC, path_prefix DESC, id ASC"
        ))
        .bind(service_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "routes"))?;

        rows.iter().map(map_route).collect()
    }

    pub async fn get_route(&self, id: &str) -> Result<Option<Route>> {
        let row = sqlx::query(&format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_db_error(e, "route"))?;

        row.as_ref().map(map_route).transpose()
    }

    pub async fn create_route(&self, input: &CreateRoute) -> Result<Route> {
        let service_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM services WHERE id = ?")
            .bind(&input.service_id)
            .fetch_one(self.pool())
            .await
            .map_err(|e| map_db_error(e, "route"))?;
        if service_exists == 0 {
            return Err(Error::NotFound(format!(
                "service '{}' not found",
                input.service_id
            )));
        }

        let id = input.id.clone().unwrap_or_else(|| new_id("rt"));
        let host = input
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_string);
        let path_prefix = input.path_prefix.clone().unwrap_or_else(|| "/".into());
        let methods = input.methods.clone().unwrap_or_default();
        let headers = input.headers.clone().unwrap_or_default();
        let protocol = input.protocol.clone().unwrap_or_else(|| "http".into());

        sqlx::query(&format!(
            "INSERT INTO routes (id, service_id, host, path_prefix, methods, headers, protocol, \
             priority, strip_prefix, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.service_id)
        .bind(&host)
        .bind(&path_prefix)
        .bind(encode_json(&methods)?)
        .bind(encode_json(&headers)?)
        .bind(&protocol)
        .bind(input.priority.unwrap_or(0))
        .bind(input.strip_prefix.unwrap_or(false))
        .bind(input.enabled.unwrap_or(true))
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "route"))?;

        self.get_route(&id)
            .await?
            .ok_or_else(|| Error::Storage("route disappeared after insert".into()))
    }

    pub async fn update_route(&self, id: &str, input: &UpdateRoute) -> Result<Route> {
        let host_param = input.host.clone();
        let methods = input.methods.as_ref().map(encode_json).transpose()?;
        let headers = input.headers.as_ref().map(encode_json).transpose()?;

        let result = sqlx::query(&format!(
            "UPDATE routes SET \
                service_id = COALESCE(?, service_id), \
                host = CASE WHEN ?2 THEN ?3 ELSE host END, \
                path_prefix = COALESCE(?4, path_prefix), \
                methods = COALESCE(?5, methods), \
                headers = COALESCE(?6, headers), \
                protocol = COALESCE(?7, protocol), \
                priority = COALESCE(?8, priority), \
                strip_prefix = COALESCE(?9, strip_prefix), \
                enabled = COALESCE(?10, enabled), \
                updated_at = {now} \
             WHERE id = ?11",
            now = now_expr()
        ))
        .bind(&input.service_id)
        .bind(host_param.is_some())
        .bind(
            host_param
                .flatten()
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty()),
        )
        .bind(&input.path_prefix)
        .bind(methods)
        .bind(headers)
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(input.strip_prefix)
        .bind(input.enabled)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "route"))?;

        affected(result.rows_affected(), "route")?;
        self.get_route(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: route not found".into()))
    }

    pub async fn delete_route(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM routes WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "route"))?;
        affected(result.rows_affected(), "route")
    }

    // Upstreams

    pub async fn list_upstreams(
        &self,
        service_id: Option<&str>,
    ) -> Result<Vec<octra_api_core::Upstream>> {
        let rows = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams \
             WHERE (?1 IS NULL OR service_id = ?1) ORDER BY name ASC, id ASC"
        ))
        .bind(service_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstreams"))?;

        rows.iter().map(map_upstream).collect()
    }

    pub async fn get_upstream(&self, id: &str) -> Result<Option<octra_api_core::Upstream>> {
        let row = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        row.as_ref().map(map_upstream).transpose()
    }

    pub async fn create_upstream(
        &self,
        input: &CreateUpstream,
    ) -> Result<octra_api_core::Upstream> {
        self.ensure_parent("services", &input.service_id, "service")
            .await?;

        let id = input.id.clone().unwrap_or_else(|| new_id("up"));
        let algorithm = input
            .algorithm
            .as_deref()
            .map(parse_algorithm)
            .transpose()?
            .unwrap_or_default();

        sqlx::query(&format!(
            "INSERT INTO upstreams (id, service_id, name, algorithm, created_at, updated_at) \
             VALUES (?, ?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.service_id)
        .bind(&input.name)
        .bind(algorithm.as_str())
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        self.get_upstream(&id)
            .await?
            .ok_or_else(|| Error::Storage("upstream disappeared after insert".into()))
    }

    pub async fn update_upstream(
        &self,
        id: &str,
        input: &UpdateUpstream,
    ) -> Result<octra_api_core::Upstream> {
        if let Some(service_id) = &input.service_id {
            self.ensure_parent("services", service_id, "service")
                .await?;
        }
        let algorithm = input
            .algorithm
            .as_deref()
            .map(parse_algorithm)
            .transpose()?
            .map(LoadBalanceAlgorithm::as_str);

        let result = sqlx::query(&format!(
            "UPDATE upstreams SET \
                service_id = COALESCE(?, service_id), \
                name = COALESCE(?, name), \
                algorithm = COALESCE(?, algorithm), \
                updated_at = {now} \
             WHERE id = ?",
            now = now_expr()
        ))
        .bind(&input.service_id)
        .bind(&input.name)
        .bind(algorithm)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        affected(result.rows_affected(), "upstream")?;
        self.get_upstream(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: upstream not found".into()))
    }

    pub async fn delete_upstream(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM upstreams WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "upstream"))?;
        affected(result.rows_affected(), "upstream")
    }

    // Targets

    pub async fn list_targets(&self, upstream_id: Option<&str>) -> Result<Vec<Target>> {
        let rows = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM targets \
             WHERE (?1 IS NULL OR upstream_id = ?1) ORDER BY id ASC"
        ))
        .bind(upstream_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "targets"))?;

        rows.iter().map(map_target).collect()
    }

    pub async fn get_target(&self, id: &str) -> Result<Option<Target>> {
        let row = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM targets WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;

        row.as_ref().map(map_target).transpose()
    }

    pub async fn create_target(&self, input: &CreateTarget) -> Result<Target> {
        self.ensure_parent("upstreams", &input.upstream_id, "upstream")
            .await?;

        let id = input.id.clone().unwrap_or_else(|| new_id("tgt"));
        sqlx::query(&format!(
            "INSERT INTO targets (id, upstream_id, url, weight, healthy, enabled, created_at, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.upstream_id)
        .bind(input.url.trim())
        .bind(input.weight.unwrap_or(100))
        .bind(input.healthy.unwrap_or(true))
        .bind(input.enabled.unwrap_or(true))
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;

        self.get_target(&id)
            .await?
            .ok_or_else(|| Error::Storage("target disappeared after insert".into()))
    }

    pub async fn update_target(&self, id: &str, input: &UpdateTarget) -> Result<Target> {
        if let Some(upstream_id) = &input.upstream_id {
            self.ensure_parent("upstreams", upstream_id, "upstream")
                .await?;
        }

        let result = sqlx::query(&format!(
            "UPDATE targets SET \
                upstream_id = COALESCE(?, upstream_id), \
                url = COALESCE(?, url), \
                weight = COALESCE(?, weight), \
                healthy = COALESCE(?, healthy), \
                enabled = COALESCE(?, enabled), \
                updated_at = {now} \
             WHERE id = ?",
            now = now_expr()
        ))
        .bind(&input.upstream_id)
        .bind(input.url.as_deref().map(str::trim))
        .bind(input.weight)
        .bind(input.healthy)
        .bind(input.enabled)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;

        affected(result.rows_affected(), "target")?;
        self.get_target(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: target not found".into()))
    }

    pub async fn delete_target(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM targets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "target"))?;
        affected(result.rows_affected(), "target")
    }

    // Consumers

    pub async fn list_consumers(&self) -> Result<Vec<Consumer>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers ORDER BY name ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumers"))?;
        rows.iter().map(map_consumer).collect()
    }

    pub async fn get_consumer(&self, id: &str) -> Result<Option<Consumer>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;
        row.as_ref().map(map_consumer).transpose()
    }

    pub async fn create_consumer(&self, input: &CreateConsumer) -> Result<Consumer> {
        let id = input.id.clone().unwrap_or_else(|| new_id("con"));
        let kind = input.kind.clone().unwrap_or_else(|| "user".into());
        sqlx::query(&format!(
            "INSERT INTO consumers (id, name, kind, created_at, updated_at) \
             VALUES (?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&kind)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;

        self.get_consumer(&id)
            .await?
            .ok_or_else(|| Error::Storage("consumer disappeared after insert".into()))
    }

    pub async fn update_consumer(&self, id: &str, input: &UpdateConsumer) -> Result<Consumer> {
        let result = sqlx::query(&format!(
            "UPDATE consumers SET \
                name = COALESCE(?, name), \
                kind = COALESCE(?, kind), \
                updated_at = {now} \
             WHERE id = ?",
            now = now_expr()
        ))
        .bind(&input.name)
        .bind(&input.kind)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;
        affected(result.rows_affected(), "consumer")?;
        self.get_consumer(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: consumer not found".into()))
    }

    pub async fn delete_consumer(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM consumers WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "consumer"))?;
        affected(result.rows_affected(), "consumer")
    }

    // Policies

    pub async fn list_policies(&self) -> Result<Vec<Policy>> {
        let rows = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policies"))?;
        rows.iter().map(map_policy).collect()
    }

    pub async fn get_policy(&self, id: &str) -> Result<Option<Policy>> {
        let row = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        row.as_ref().map(map_policy).transpose()
    }

    pub async fn create_policy(&self, input: &CreatePolicy) -> Result<Policy> {
        let id = input.id.clone().unwrap_or_else(|| new_id("pol"));
        let scope = input.scope.clone().unwrap_or_else(|| {
            if input.consumer_id.is_some() {
                "consumer".into()
            } else {
                "global".into()
            }
        });
        let document = serde_json::to_string(
            input
                .document
                .as_ref()
                .unwrap_or(&serde_json::Value::Object(Default::default())),
        )
        .map_err(|e| Error::Storage(format!("cannot serialize policy document: {e}")))?;

        sqlx::query(&format!(
            "INSERT INTO policies (id, name, scope, consumer_id, document, created_at, updated_at)              VALUES (?, ?, ?, ?, ?, {now}, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&scope)
        .bind(&input.consumer_id)
        .bind(document)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;

        self.get_policy(&id)
            .await?
            .ok_or_else(|| Error::Storage("policy disappeared after insert".into()))
    }

    pub async fn update_policy(&self, id: &str, input: &UpdatePolicy) -> Result<Policy> {
        let document = input
            .document
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| Error::Storage(format!("cannot serialize policy document: {e}")))?;
        let result = sqlx::query(&format!(
            "UPDATE policies SET                 name = COALESCE(?, name),                 scope = COALESCE(?, scope),                 consumer_id = COALESCE(?, consumer_id),                 document = COALESCE(?, document),                 updated_at = {now}              WHERE id = ?",
            now = now_expr()
        ))
        .bind(&input.name)
        .bind(&input.scope)
        .bind(&input.consumer_id)
        .bind(document)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        affected(result.rows_affected(), "policy")?;
        self.get_policy(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: policy not found".into()))
    }

    pub async fn delete_policy(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM policies WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "policy"))?;
        affected(result.rows_affected(), "policy")
    }

    // Credentials

    pub async fn list_credentials(&self, consumer_id: Option<&str>) -> Result<Vec<ApiCredential>> {
        let rows = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials \
             WHERE (?1 IS NULL OR consumer_id = ?1) ORDER BY created_at ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter().map(map_credential).collect()
    }

    pub async fn get_credential(&self, id: &str) -> Result<Option<ApiCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref().map(map_credential).transpose()
    }

    /// Creates a credential. The plaintext key is represented only by its
    /// prefix and SHA-256 hash.
    pub async fn create_credential(
        &self,
        input: &CreateApiCredential,
        key_prefix: &str,
        key_hash: &str,
    ) -> Result<ApiCredential> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;

        let id = input.id.clone().unwrap_or_else(|| new_id("cred"));
        let kind = input.kind.clone().unwrap_or_else(|| "api_key".into());
        let name = input.name.clone().unwrap_or_else(|| key_prefix.to_string());
        let secret_ref = format!("credential:{id}");

        sqlx::query(&format!(
            "INSERT INTO credentials (id, consumer_id, kind, secret_ref, name, key_prefix, \
             key_hash, enabled, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {now})",
            now = now_expr()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&kind)
        .bind(&secret_ref)
        .bind(&name)
        .bind(key_prefix)
        .bind(key_hash)
        .bind(input.enabled.unwrap_or(true))
        .bind(input.expires_at.map(|at| at.to_rfc3339()))
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;

        self.get_credential(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn update_credential(
        &self,
        id: &str,
        input: &UpdateApiCredential,
    ) -> Result<ApiCredential> {
        if let Some(consumer_id) = &input.consumer_id {
            self.ensure_parent("consumers", consumer_id, "consumer")
                .await?;
        }
        let expires_set = input.expires_at.is_some();
        let expires = input.expires_at.flatten().map(|at| at.to_rfc3339());

        let result = sqlx::query(
            "UPDATE credentials SET \
                consumer_id = COALESCE(?, consumer_id), \
                name = COALESCE(?, name), \
                enabled = COALESCE(?, enabled), \
                key_hash = COALESCE(?, key_hash), \
                key_prefix = COALESCE(?, key_prefix), \
                expires_at = CASE WHEN ? THEN ? ELSE expires_at END \
             WHERE id = ?",
        )
        .bind(&input.consumer_id)
        .bind(&input.name)
        .bind(input.enabled)
        .bind(&input.key_hash)
        .bind(&input.key_prefix)
        .bind(expires_set)
        .bind(expires)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_credential(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_credential(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM credentials WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    pub async fn find_credential_by_hash(&self, key_hash: &str) -> Result<Option<ApiCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE key_hash = ?"
        ))
        .bind(key_hash)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref().map(map_credential).transpose()
    }

    pub async fn touch_credential(&self, id: &str) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE credentials SET last_used_at = {now} WHERE id = ?",
            now = now_expr()
        ))
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        Ok(())
    }

    /// Loads every entity the API data plane needs in a constant number of
    /// queries (no per-service or per-upstream N+1 reads).
    pub async fn load_api_runtime_data(&self) -> Result<ApiRuntimeData> {
        let mut tx = self.begin().await?;

        let services = sqlx::query(&format!(
            "SELECT {SERVICE_COLUMNS} FROM services ORDER BY id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "services"))?;
        let services = services
            .iter()
            .map(map_service)
            .collect::<Result<Vec<_>>>()?;

        let routes = sqlx::query(&format!(
            "SELECT {ROUTE_COLUMNS} FROM routes ORDER BY priority DESC, path_prefix DESC, id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "routes"))?;
        let routes = routes.iter().map(map_route).collect::<Result<Vec<_>>>()?;

        let upstream_rows = sqlx::query("SELECT id, service_id, algorithm FROM upstreams")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| map_db_error(e, "upstreams"))?;
        let upstreams = upstream_rows
            .iter()
            .map(|row| {
                let algorithm: String = decode(row.try_get("algorithm"), "upstream")?;
                Ok(UpstreamRow {
                    id: decode(row.try_get("id"), "upstream")?,
                    service_id: decode(row.try_get("service_id"), "upstream")?,
                    algorithm: LoadBalanceAlgorithm::parse(&algorithm).ok_or_else(|| {
                        Error::Storage(format!("unknown load balance algorithm '{algorithm}'"))
                    })?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let targets = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM targets ORDER BY id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "targets"))?;
        let targets = targets.iter().map(map_target).collect::<Result<Vec<_>>>()?;

        let credentials = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE key_hash IS NOT NULL"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        let credentials = credentials
            .iter()
            .map(map_credential)
            .collect::<Result<Vec<_>>>()?;

        let policies = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies ORDER BY name ASC, id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "policies"))?;
        let policies = policies
            .iter()
            .map(map_policy)
            .collect::<Result<Vec<_>>>()?;

        let counts = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM services) AS services,
                (SELECT COUNT(*) FROM routes) AS routes,
                (SELECT COUNT(*) FROM upstreams) AS upstreams,
                (SELECT COUNT(*) FROM targets) AS targets,
                (SELECT COUNT(*) FROM consumers) AS consumers,
                (SELECT COUNT(*) FROM credentials) AS credentials",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "counts"))?;

        tx.commit()
            .await
            .map_err(|e| map_db_error(e, "runtime data"))?;

        Ok(ApiRuntimeData {
            services,
            routes,
            upstreams,
            targets,
            credentials,
            policies,
            counts: ApiRuntimeCounts {
                services: decode(counts.try_get("services"), "counts")?,
                routes: decode(counts.try_get("routes"), "counts")?,
                upstreams: decode(counts.try_get("upstreams"), "counts")?,
                targets: decode(counts.try_get("targets"), "counts")?,
                consumers: decode(counts.try_get("consumers"), "counts")?,
                credentials: decode(counts.try_get("credentials"), "counts")?,
            },
        })
    }

    pub async fn refresh_api_runtime_data(&self) -> Result<ApiRuntimeData> {
        self.load_api_runtime_data().await
    }

    async fn begin(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool()
            .begin()
            .await
            .map_err(|e| map_db_error(e, "transaction"))
    }

    async fn ensure_parent(&self, table: &str, id: &str, entity: &str) -> Result<()> {
        let exists: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE id = ?"))
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(|e| map_db_error(e, entity))?;
        if exists == 0 {
            return Err(Error::NotFound(format!("{entity} '{id}' not found")));
        }
        Ok(())
    }
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| Error::Storage(format!("cannot encode JSON payload: {err}")))
}

fn parse_algorithm(value: &str) -> Result<LoadBalanceAlgorithm> {
    LoadBalanceAlgorithm::parse(value)
        .ok_or_else(|| Error::Storage(format!("unknown load balance algorithm '{value}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_api_core::CreateApiCredential;

    async fn setup() -> Storage {
        Storage::connect_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn service_crud_and_unique_conflict() {
        let storage = setup().await;
        let service = storage
            .create_service(&CreateService {
                name: "orders".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(service.require_auth);
        assert_eq!(service.retries, 0);

        let duplicate = storage
            .create_service(&CreateService {
                id: Some("svc-2".into()),
                name: "orders".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(duplicate, Err(Error::Storage(ref msg)) if msg.contains("conflict")));

        let updated = storage
            .update_service(
                &service.id,
                &UpdateService {
                    retries: Some(2),
                    require_auth: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.retries, 2);
        assert!(!updated.require_auth);

        storage.delete_service(&service.id).await.unwrap();
        assert!(storage.get_service(&service.id).await.unwrap().is_none());
        assert!(matches!(
            storage.delete_service(&service.id).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn route_filters_and_parent_checks() {
        let storage = setup().await;
        let service = storage
            .create_service(&CreateService {
                name: "svc".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let route = storage
            .create_route(&CreateRoute {
                service_id: service.id.clone(),
                path_prefix: Some("/api".into()),
                methods: Some(vec!["GET".into()]),
                headers: Some(BTreeMap::from([("x-env".into(), "prod".into())])),
                strip_prefix: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(route.methods, vec!["GET".to_string()]);
        assert!(route.strip_prefix);
        assert_eq!(route.headers.get("x-env").map(String::as_str), Some("prod"));

        assert_eq!(
            storage.list_routes(Some(&service.id)).await.unwrap().len(),
            1
        );
        assert_eq!(storage.list_routes(Some("missing")).await.unwrap().len(), 0);

        let missing = storage
            .create_route(&CreateRoute {
                service_id: "missing".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(missing, Err(Error::NotFound(_))));

        let cleared = storage
            .update_route(
                &route.id,
                &UpdateRoute {
                    methods: Some(vec![]),
                    priority: Some(7),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(cleared.methods.is_empty());
        assert_eq!(cleared.priority, 7);
    }

    #[tokio::test]
    async fn targets_credentials_and_runtime_loading() {
        let storage = setup().await;
        let service = storage
            .create_service(&CreateService {
                name: "svc".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let upstream = storage
            .create_upstream(&CreateUpstream {
                service_id: service.id.clone(),
                name: "primary".into(),
                algorithm: Some("weighted_round_robin".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(upstream.algorithm, LoadBalanceAlgorithm::WeightedRoundRobin);

        storage
            .create_target(&CreateTarget {
                upstream_id: upstream.id.clone(),
                url: "http://127.0.0.1:9000".into(),
                weight: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            storage
                .list_targets(Some(&upstream.id))
                .await
                .unwrap()
                .len(),
            1
        );

        let consumer = storage
            .create_consumer(&CreateConsumer {
                name: "team".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let credential = storage
            .create_credential(
                &CreateApiCredential {
                    consumer_id: consumer.id.clone(),
                    ..Default::default()
                },
                "ok_abcd1234",
                "deadbeef",
            )
            .await
            .unwrap();
        assert_eq!(credential.key_hash.as_deref(), Some("deadbeef"));

        let duplicate_hash = storage
            .create_credential(
                &CreateApiCredential {
                    consumer_id: consumer.id.clone(),
                    ..Default::default()
                },
                "ok_other",
                "deadbeef",
            )
            .await;
        assert!(matches!(duplicate_hash, Err(Error::Storage(ref m)) if m.contains("conflict")));

        let found = storage
            .find_credential_by_hash("deadbeef")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, credential.id);

        let data = storage.load_api_runtime_data().await.unwrap();
        assert_eq!(data.services.len(), 1);
        assert_eq!(data.routes.len(), 0);
        assert_eq!(data.upstreams.len(), 1);
        assert_eq!(data.targets.len(), 1);
        assert_eq!(data.credentials.len(), 1);
        assert_eq!(data.counts.consumers, 1);
    }
}
