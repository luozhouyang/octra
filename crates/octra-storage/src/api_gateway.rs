//! API gateway repositories: services, routes, upstreams, targets, consumers,
//! plugin instances, and authentication credentials.

use std::collections::BTreeMap;

use octra_api_core::{
    Acl, BasicauthCredential, Consumer, ConsumerGroup, ConsumerGroupMember, CreateAcl,
    CreateBasicauthCredential, CreateConsumer, CreateConsumerGroup, CreateJwtSecret,
    CreateKeyauthCredential, CreatePluginInstance, CreateRoute, CreateService, CreateTarget,
    CreateUpstream, JwtSecret, KeyauthCredential, LoadBalanceAlgorithm, PluginInstance, Route,
    Service, Target, UpdateAcl, UpdateBasicauthCredential, UpdateConsumer, UpdateConsumerGroup,
    UpdateJwtSecret, UpdateKeyauthCredential, UpdatePluginInstance, UpdateRoute, UpdateService,
    UpdateTarget, UpdateUpstream, Upstream,
};
use octra_core::{Error, Result};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

const SERVICE_COLUMNS: &str = "id, name, protocol, host, port, path, retries, connect_timeout, \
     write_timeout, read_timeout, enabled, description, created_at, updated_at";
const ROUTE_COLUMNS: &str = "id, name, protocols, methods, hosts, paths, headers, strip_path, \
     preserve_host, request_buffering, response_buffering, regex_priority, service_id, enabled, \
     tags, created_at, updated_at";
const UPSTREAM_COLUMNS: &str = "id, name, algorithm, hash_on, hash_fallback, slots, healthchecks, \
     host_header, tags, created_at, updated_at";
const TARGET_COLUMNS: &str = "id, upstream_id, target, weight, tags, created_at, updated_at";
const CONSUMER_COLUMNS: &str = "id, username, custom_id, tags, created_at, updated_at";
const PLUGIN_COLUMNS: &str = "id, name, instance_name, config, enabled, protocols, ordering, \
     service_id, route_id, consumer_id, consumer_group_id, tags, created_at, updated_at";
const CONSUMER_GROUP_COLUMNS: &str = "id, name, tags, created_at, updated_at";
const CONSUMER_GROUP_MEMBER_COLUMNS: &str = "consumer_id, consumer_group_id, created_at";
const KEYAUTH_COLUMNS: &str = "id, consumer_id, key, ttl, tags, created_at";
const BASICAUTH_COLUMNS: &str = "id, consumer_id, username, password, tags, created_at";
const JWT_COLUMNS: &str =
    "id, consumer_id, key, secret, algorithm, rsa_public_key, tags, created_at";
const ACL_COLUMNS: &str = "id, consumer_id, \"group\", tags, created_at";

const SQLITE_CONSTRAINT_UNIQUE: &str = "2067";
const SQLITE_CONSTRAINT_PRIMARYKEY: &str = "1555";
const SQLITE_CONSTRAINT_FOREIGNKEY: &str = "787";

/// One coherent view of everything the API data plane needs at runtime.
#[derive(Debug, Clone, Default)]
pub struct ApiRuntimeData {
    pub services: Vec<Service>,
    pub routes: Vec<Route>,
    pub upstreams: Vec<Upstream>,
    pub targets: Vec<Target>,
    pub consumers: Vec<Consumer>,
    pub consumer_groups: Vec<ConsumerGroup>,
    pub consumer_group_members: Vec<ConsumerGroupMember>,
    pub keyauth: Vec<KeyauthCredential>,
    pub basicauth: Vec<BasicauthCredential>,
    pub jwt: Vec<JwtSecret>,
    pub acls: Vec<Acl>,
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
    pub plugins: i64,
}

fn epoch_now() -> &'static str {
    "CAST(strftime('%s', 'now') AS INTEGER)"
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

/// Unwraps a row decode result, mapping failures to storage errors.
pub(crate) fn decode<T>(value: sqlx::error::Result<T>, entity: &str) -> Result<T> {
    value.map_err(|e| map_db_error(e, entity))
}

pub(crate) fn affected(rows: u64, entity: &str) -> Result<()> {
    if rows == 0 {
        Err(Error::NotFound(format!("{entity} not found")))
    } else {
        Ok(())
    }
}

pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| Error::Storage(format!("cannot encode JSON payload: {err}")))
}

fn encode_optional_json<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value.map(encode_json).transpose()
}

fn decode_optional_json<T: serde::de::DeserializeOwned>(
    value: Option<String>,
    field: &str,
) -> Result<Option<T>> {
    value
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|err| Error::Storage(format!("invalid {field} JSON '{raw}': {err}")))
        })
        .transpose()
}

fn decode_json_or_default<T: serde::de::DeserializeOwned + Default>(
    value: Option<String>,
    field: &str,
) -> Result<T> {
    Ok(decode_optional_json(value, field)?.unwrap_or_default())
}

fn map_service(row: &SqliteRow) -> Result<Service> {
    Ok(Service {
        id: decode(row.try_get("id"), "service")?,
        name: decode(row.try_get("name"), "service")?,
        protocol: decode(row.try_get("protocol"), "service")?,
        host: decode(row.try_get("host"), "service")?,
        port: decode(row.try_get("port"), "service")?,
        path: decode(row.try_get("path"), "service")?,
        retries: decode(row.try_get("retries"), "service")?,
        connect_timeout: decode(row.try_get("connect_timeout"), "service")?,
        write_timeout: decode(row.try_get("write_timeout"), "service")?,
        read_timeout: decode(row.try_get("read_timeout"), "service")?,
        enabled: decode::<i64>(row.try_get("enabled"), "service")? != 0,
        description: decode(row.try_get("description"), "service")?,
        created_at: decode(row.try_get("created_at"), "service")?,
        updated_at: decode(row.try_get("updated_at"), "service")?,
    })
}

fn map_route(row: &SqliteRow) -> Result<Route> {
    Ok(Route {
        id: decode(row.try_get("id"), "route")?,
        name: decode(row.try_get("name"), "route")?,
        protocols: decode_json_or_default(
            row.try_get::<Option<String>, _>("protocols").ok().flatten(),
            "protocols",
        )?,
        methods: decode_json_or_default(
            row.try_get::<Option<String>, _>("methods").ok().flatten(),
            "methods",
        )?,
        hosts: decode_json_or_default(
            row.try_get::<Option<String>, _>("hosts").ok().flatten(),
            "hosts",
        )?,
        paths: decode_json_or_default(
            row.try_get::<Option<String>, _>("paths").ok().flatten(),
            "paths",
        )?,
        headers: decode_json_or_default(
            row.try_get::<Option<String>, _>("headers").ok().flatten(),
            "headers",
        )?,
        strip_path: decode::<i64>(row.try_get("strip_path"), "route")? != 0,
        preserve_host: decode::<i64>(row.try_get("preserve_host"), "route")? != 0,
        request_buffering: decode::<i64>(row.try_get("request_buffering"), "route")? != 0,
        response_buffering: decode::<i64>(row.try_get("response_buffering"), "route")? != 0,
        regex_priority: decode(row.try_get("regex_priority"), "route")?,
        service_id: decode(row.try_get("service_id"), "route")?,
        enabled: decode::<i64>(row.try_get("enabled"), "route")? != 0,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "route")?,
        updated_at: decode(row.try_get("updated_at"), "route")?,
    })
}

fn map_upstream(row: &SqliteRow) -> Result<Upstream> {
    let algorithm: String = decode(row.try_get("algorithm"), "upstream")?;
    Ok(Upstream {
        id: decode(row.try_get("id"), "upstream")?,
        name: decode(row.try_get("name"), "upstream")?,
        algorithm: LoadBalanceAlgorithm::parse(&algorithm).ok_or_else(|| {
            Error::Storage(format!("unknown load balance algorithm '{algorithm}'"))
        })?,
        hash_on: decode(row.try_get("hash_on"), "upstream")?,
        hash_fallback: decode(row.try_get("hash_fallback"), "upstream")?,
        slots: decode(row.try_get("slots"), "upstream")?,
        healthchecks: decode_optional_json(
            row.try_get::<Option<String>, _>("healthchecks")
                .ok()
                .flatten(),
            "healthchecks",
        )?
        .unwrap_or(serde_json::json!({})),
        host_header: decode(row.try_get("host_header"), "upstream")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "upstream")?,
        updated_at: decode(row.try_get("updated_at"), "upstream")?,
    })
}

fn map_target(row: &SqliteRow) -> Result<Target> {
    let weight = u32::try_from(decode::<i64>(row.try_get("weight"), "target")?)
        .map_err(|_| Error::Storage("target weight out of range".into()))?;
    Ok(Target {
        id: decode(row.try_get("id"), "target")?,
        upstream_id: decode(row.try_get("upstream_id"), "target")?,
        target: decode(row.try_get("target"), "target")?,
        weight,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "target")?,
        updated_at: decode(row.try_get("updated_at"), "target")?,
    })
}

fn map_consumer(row: &SqliteRow) -> Result<Consumer> {
    Ok(Consumer {
        id: decode(row.try_get("id"), "consumer")?,
        username: decode(row.try_get("username"), "consumer")?,
        custom_id: decode(row.try_get("custom_id"), "consumer")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "consumer")?,
        updated_at: decode(row.try_get("updated_at"), "consumer")?,
    })
}

fn map_consumer_group(row: &SqliteRow) -> Result<ConsumerGroup> {
    Ok(ConsumerGroup {
        id: decode(row.try_get("id"), "consumer group")?,
        name: decode(row.try_get("name"), "consumer group")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "consumer group")?,
        updated_at: decode(row.try_get("updated_at"), "consumer group")?,
    })
}

fn map_consumer_group_member(row: &SqliteRow) -> Result<ConsumerGroupMember> {
    Ok(ConsumerGroupMember {
        consumer_id: decode(row.try_get("consumer_id"), "consumer group member")?,
        consumer_group_id: decode(row.try_get("consumer_group_id"), "consumer group member")?,
        created_at: decode(row.try_get("created_at"), "consumer group member")?,
    })
}

fn map_plugin(row: &SqliteRow) -> Result<PluginInstance> {
    Ok(PluginInstance {
        id: decode(row.try_get("id"), "plugin")?,
        name: decode(row.try_get("name"), "plugin")?,
        instance_name: decode(row.try_get("instance_name"), "plugin")?,
        config: decode_optional_json(
            row.try_get::<Option<String>, _>("config").ok().flatten(),
            "config",
        )?
        .unwrap_or(serde_json::json!({})),
        enabled: decode::<i64>(row.try_get("enabled"), "plugin")? != 0,
        protocols: decode_json_or_default(
            row.try_get::<Option<String>, _>("protocols").ok().flatten(),
            "protocols",
        )?,
        ordering: decode_optional_json(
            row.try_get::<Option<String>, _>("ordering").ok().flatten(),
            "ordering",
        )?,
        service_id: decode(row.try_get("service_id"), "plugin")?,
        route_id: decode(row.try_get("route_id"), "plugin")?,
        consumer_id: decode(row.try_get("consumer_id"), "plugin")?,
        consumer_group_id: decode(row.try_get("consumer_group_id"), "plugin")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "plugin")?,
        updated_at: decode(row.try_get("updated_at"), "plugin")?,
    })
}

fn map_keyauth(row: &SqliteRow) -> Result<KeyauthCredential> {
    Ok(KeyauthCredential {
        id: decode(row.try_get("id"), "credential")?,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        key: decode(row.try_get("key"), "credential")?,
        ttl: decode(row.try_get("ttl"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

fn map_basicauth(row: &SqliteRow) -> Result<BasicauthCredential> {
    Ok(BasicauthCredential {
        id: decode(row.try_get("id"), "credential")?,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        username: decode(row.try_get("username"), "credential")?,
        password: decode(row.try_get("password"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

fn map_jwt(row: &SqliteRow) -> Result<JwtSecret> {
    Ok(JwtSecret {
        id: decode(row.try_get("id"), "credential")?,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        key: decode(row.try_get("key"), "credential")?,
        secret: decode(row.try_get("secret"), "credential")?,
        algorithm: decode(row.try_get("algorithm"), "credential")?,
        rsa_public_key: decode(row.try_get("rsa_public_key"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

fn map_acl(row: &SqliteRow) -> Result<Acl> {
    Ok(Acl {
        id: decode(row.try_get("id"), "acl")?,
        consumer_id: decode(row.try_get("consumer_id"), "acl")?,
        group: decode(row.try_get("group"), "acl")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "acl")?,
    })
}

pub(crate) fn map_policy(row: &SqliteRow) -> Result<octra_api_core::Policy> {
    let document: String = decode(row.try_get("document"), "policy")?;
    Ok(octra_api_core::Policy {
        id: decode(row.try_get("id"), "policy")?,
        name: decode(row.try_get("name"), "policy")?,
        scope: decode(row.try_get("scope"), "policy")?,
        consumer_id: decode(row.try_get("consumer_id"), "policy")?,
        document: serde_json::from_str(&document)
            .map_err(|error| Error::Storage(format!("invalid policy document: {error}")))?,
        created_at: parse_rfc3339(&decode::<String>(row.try_get("created_at"), "policy")?)?,
        updated_at: parse_rfc3339(&decode::<String>(row.try_get("updated_at"), "policy")?)?,
    })
}

fn parse_rfc3339(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|error| Error::Storage(format!("invalid timestamp '{value}': {error}")))
}

fn text_now() -> &'static str {
    "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
}

#[derive(Clone)]
pub struct ApiStore {
    pool: SqlitePool,
}

impl ApiStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // Policies (LLM authorization documents)

    pub async fn create_policy(
        &self,
        input: &octra_api_core::CreatePolicy,
    ) -> Result<octra_api_core::Policy> {
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
        .map_err(|error| Error::Storage(format!("cannot serialize policy document: {error}")))?;
        sqlx::query(&format!(
            "INSERT INTO policies (id, name, scope, consumer_id, document, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, {now}, {now})",
            now = text_now()
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

    pub async fn get_policy(&self, id: &str) -> Result<Option<octra_api_core::Policy>> {
        let row = sqlx::query(
            "SELECT id, name, scope, consumer_id, document, created_at, updated_at \
             FROM policies WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        row.as_ref().map(map_policy).transpose()
    }

    pub async fn update_policy(
        &self,
        id: &str,
        input: &octra_api_core::UpdatePolicy,
    ) -> Result<octra_api_core::Policy> {
        let document = input
            .document
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                Error::Storage(format!("cannot serialize policy document: {error}"))
            })?;
        let result = sqlx::query(&format!(
            "UPDATE policies SET \
                name = COALESCE(?, name), \
                scope = COALESCE(?, scope), \
                consumer_id = COALESCE(?, consumer_id), \
                document = COALESCE(?, document), \
                updated_at = {now} \
             WHERE id = ?",
            now = text_now()
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

    pub async fn list_policies(&self) -> Result<Vec<octra_api_core::Policy>> {
        let rows = sqlx::query(
            "SELECT id, name, scope, consumer_id, document, created_at, updated_at \
             FROM policies ORDER BY name ASC, id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policies"))?;
        rows.iter().map(map_policy).collect()
    }

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
        let host = input
            .host
            .clone()
            .filter(|host| !host.trim().is_empty())
            .unwrap_or_else(|| input.name.clone());

        sqlx::query(&format!(
            "INSERT INTO services (id, name, protocol, host, port, path, retries, \
             connect_timeout, write_timeout, read_timeout, enabled, description, created_at, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&protocol)
        .bind(&host)
        .bind(input.port.unwrap_or(80))
        .bind(&input.path)
        .bind(input.retries.unwrap_or(5))
        .bind(input.connect_timeout.unwrap_or(60_000))
        .bind(input.write_timeout.unwrap_or(60_000))
        .bind(input.read_timeout.unwrap_or(60_000))
        .bind(input.enabled.unwrap_or(true))
        .bind(input.description.clone().unwrap_or_default())
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        self.get_service(&id)
            .await?
            .ok_or_else(|| Error::Storage("service disappeared after insert".into()))
    }

    pub async fn update_service(&self, id: &str, input: &UpdateService) -> Result<Service> {
        let path_set = input.path.is_some();
        let path = input.path.clone().flatten();
        let result = sqlx::query(&format!(
            "UPDATE services SET \
                name = COALESCE(?, name), \
                protocol = COALESCE(?, protocol), \
                host = COALESCE(?, host), \
                port = COALESCE(?, port), \
                path = CASE WHEN ? THEN ? ELSE path END, \
                retries = COALESCE(?, retries), \
                connect_timeout = COALESCE(?, connect_timeout), \
                write_timeout = COALESCE(?, write_timeout), \
                read_timeout = COALESCE(?, read_timeout), \
                enabled = COALESCE(?, enabled), \
                description = COALESCE(?, description), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(&input.protocol)
        .bind(&input.host)
        .bind(input.port)
        .bind(path_set)
        .bind(path)
        .bind(input.retries)
        .bind(input.connect_timeout)
        .bind(input.write_timeout)
        .bind(input.read_timeout)
        .bind(input.enabled)
        .bind(&input.description)
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
             ORDER BY regex_priority DESC, created_at ASC, id ASC"
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
        if let Some(service_id) = &input.service_id {
            self.ensure_parent("services", service_id, "service")
                .await?;
        }

        let id = input.id.clone().unwrap_or_else(|| new_id("rt"));
        let protocols = input
            .protocols
            .clone()
            .unwrap_or_else(|| vec!["http".into(), "https".into()]);

        sqlx::query(&format!(
            "INSERT INTO routes (id, name, protocols, methods, hosts, paths, headers, \
             strip_path, preserve_host, request_buffering, response_buffering, regex_priority, \
             service_id, enabled, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(encode_json(&protocols)?)
        .bind(encode_optional_json(input.methods.as_ref())?)
        .bind(encode_optional_json(input.hosts.as_ref())?)
        .bind(encode_optional_json(input.paths.as_ref())?)
        .bind(encode_optional_json(input.headers.as_ref())?)
        .bind(input.strip_path.unwrap_or(true))
        .bind(input.preserve_host.unwrap_or(false))
        .bind(input.request_buffering.unwrap_or(true))
        .bind(input.response_buffering.unwrap_or(true))
        .bind(input.regex_priority.unwrap_or(0))
        .bind(&input.service_id)
        .bind(input.enabled.unwrap_or(true))
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "route"))?;

        self.get_route(&id)
            .await?
            .ok_or_else(|| Error::Storage("route disappeared after insert".into()))
    }

    pub async fn update_route(&self, id: &str, input: &UpdateRoute) -> Result<Route> {
        let name_set = input.name.is_some();
        let name = input.name.clone().flatten();
        let service_set = input.service_id.is_some();
        let service_id = input.service_id.clone().flatten();

        let result = sqlx::query(&format!(
            "UPDATE routes SET \
                name = CASE WHEN ? THEN ? ELSE name END, \
                protocols = COALESCE(?, protocols), \
                methods = COALESCE(?, methods), \
                hosts = COALESCE(?, hosts), \
                paths = COALESCE(?, paths), \
                headers = COALESCE(?, headers), \
                strip_path = COALESCE(?, strip_path), \
                preserve_host = COALESCE(?, preserve_host), \
                request_buffering = COALESCE(?, request_buffering), \
                response_buffering = COALESCE(?, response_buffering), \
                regex_priority = COALESCE(?, regex_priority), \
                service_id = CASE WHEN ? THEN ? ELSE service_id END, \
                enabled = COALESCE(?, enabled), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(name_set)
        .bind(name)
        .bind(input.protocols.as_ref().map(encode_json).transpose()?)
        .bind(input.methods.as_ref().map(encode_json).transpose()?)
        .bind(input.hosts.as_ref().map(encode_json).transpose()?)
        .bind(input.paths.as_ref().map(encode_json).transpose()?)
        .bind(input.headers.as_ref().map(encode_json).transpose()?)
        .bind(input.strip_path)
        .bind(input.preserve_host)
        .bind(input.request_buffering)
        .bind(input.response_buffering)
        .bind(input.regex_priority)
        .bind(service_set)
        .bind(service_id)
        .bind(input.enabled)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
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

    pub async fn list_upstreams(&self) -> Result<Vec<Upstream>> {
        let rows = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstreams"))?;
        rows.iter().map(map_upstream).collect()
    }

    pub async fn get_upstream(&self, id: &str) -> Result<Option<Upstream>> {
        let row = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;
        row.as_ref().map(map_upstream).transpose()
    }

    pub async fn create_upstream(&self, input: &CreateUpstream) -> Result<Upstream> {
        let id = input.id.clone().unwrap_or_else(|| new_id("up"));
        let algorithm = input
            .algorithm
            .as_deref()
            .map(|value| {
                LoadBalanceAlgorithm::parse(value).ok_or_else(|| {
                    Error::Storage(format!("unknown load balance algorithm '{value}'"))
                })
            })
            .transpose()?
            .unwrap_or_default();

        sqlx::query(&format!(
            "INSERT INTO upstreams (id, name, algorithm, hash_on, hash_fallback, slots, \
             healthchecks, host_header, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(algorithm.as_str())
        .bind(input.hash_on.clone().unwrap_or_else(|| "none".into()))
        .bind(input.hash_fallback.clone().unwrap_or_else(|| "none".into()))
        .bind(input.slots.unwrap_or(10_000))
        .bind(encode_json(
            input
                .healthchecks
                .as_ref()
                .unwrap_or(&serde_json::json!({})),
        )?)
        .bind(&input.host_header)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        self.get_upstream(&id)
            .await?
            .ok_or_else(|| Error::Storage("upstream disappeared after insert".into()))
    }

    pub async fn update_upstream(&self, id: &str, input: &UpdateUpstream) -> Result<Upstream> {
        let algorithm = input
            .algorithm
            .as_deref()
            .map(|value| {
                LoadBalanceAlgorithm::parse(value)
                    .map(LoadBalanceAlgorithm::as_str)
                    .ok_or_else(|| {
                        Error::Storage(format!("unknown load balance algorithm '{value}'"))
                    })
            })
            .transpose()?;

        let result = sqlx::query(&format!(
            "UPDATE upstreams SET \
                name = COALESCE(?, name), \
                algorithm = COALESCE(?, algorithm), \
                hash_on = COALESCE(?, hash_on), \
                hash_fallback = COALESCE(?, hash_fallback), \
                slots = COALESCE(?, slots), \
                healthchecks = COALESCE(?, healthchecks), \
                host_header = COALESCE(?, host_header), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(algorithm)
        .bind(&input.hash_on)
        .bind(&input.hash_fallback)
        .bind(input.slots)
        .bind(input.healthchecks.as_ref().map(encode_json).transpose()?)
        .bind(&input.host_header)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
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
             WHERE (?1 IS NULL OR upstream_id = ?1) ORDER BY created_at ASC, id ASC"
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
            "INSERT INTO targets (id, upstream_id, target, weight, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.upstream_id)
        .bind(input.target.trim())
        .bind(input.weight.unwrap_or(100))
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
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
                target = COALESCE(?, target), \
                weight = COALESCE(?, weight), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.upstream_id)
        .bind(input.target.as_deref().map(str::trim))
        .bind(input.weight)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
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
            "SELECT {CONSUMER_COLUMNS} FROM consumers ORDER BY username ASC, id ASC"
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
        sqlx::query(&format!(
            "INSERT INTO consumers (id, username, custom_id, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.username)
        .bind(&input.custom_id)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
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
                username = COALESCE(?, username), \
                custom_id = COALESCE(?, custom_id), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.username)
        .bind(&input.custom_id)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
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

    // Consumer groups

    pub async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroup>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer groups"))?;
        rows.iter().map(map_consumer_group).collect()
    }

    pub async fn get_consumer_group(&self, id: &str) -> Result<Option<ConsumerGroup>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        row.as_ref().map(map_consumer_group).transpose()
    }

    pub async fn get_consumer_group_by_name(&self, name: &str) -> Result<Option<ConsumerGroup>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        row.as_ref().map(map_consumer_group).transpose()
    }

    pub async fn create_consumer_group(
        &self,
        input: &CreateConsumerGroup,
    ) -> Result<ConsumerGroup> {
        let id = input.id.clone().unwrap_or_else(|| new_id("cg"));
        sqlx::query(&format!(
            "INSERT INTO consumer_groups (id, name, tags, created_at, updated_at) \
             VALUES (?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;

        self.get_consumer_group(&id)
            .await?
            .ok_or_else(|| Error::Storage("consumer group disappeared after insert".into()))
    }

    pub async fn update_consumer_group(
        &self,
        id: &str,
        input: &UpdateConsumerGroup,
    ) -> Result<ConsumerGroup> {
        let result = sqlx::query(&format!(
            "UPDATE consumer_groups SET \
                name = COALESCE(?, name), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        affected(result.rows_affected(), "consumer group")?;
        self.get_consumer_group(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: consumer group not found".into()))
    }

    pub async fn delete_consumer_group(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM consumer_groups WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "consumer group"))?;
        affected(result.rows_affected(), "consumer group")
    }

    pub async fn list_consumer_group_members(
        &self,
        consumer_group_id: &str,
    ) -> Result<Vec<Consumer>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers \
             WHERE id IN (SELECT consumer_id FROM consumer_group_consumers \
                          WHERE consumer_group_id = ?) \
             ORDER BY username ASC, id ASC"
        ))
        .bind(consumer_group_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group members"))?;
        rows.iter().map(map_consumer).collect()
    }

    pub async fn list_consumer_groups_for_consumer(
        &self,
        consumer_id: &str,
    ) -> Result<Vec<ConsumerGroup>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups \
             WHERE id IN (SELECT consumer_group_id FROM consumer_group_consumers \
                          WHERE consumer_id = ?) \
             ORDER BY name ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer groups"))?;
        rows.iter().map(map_consumer_group).collect()
    }

    pub async fn add_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<ConsumerGroupMember> {
        self.ensure_parent("consumer_groups", consumer_group_id, "consumer group")
            .await?;
        self.ensure_parent("consumers", consumer_id, "consumer")
            .await?;
        sqlx::query(&format!(
            "INSERT INTO consumer_group_consumers \
                (consumer_id, consumer_group_id, created_at) VALUES (?, ?, {now})",
            now = epoch_now()
        ))
        .bind(consumer_id)
        .bind(consumer_group_id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;

        self.get_consumer_group_member(consumer_group_id, consumer_id)
            .await?
            .ok_or_else(|| Error::Storage("consumer group member disappeared after insert".into()))
    }

    pub async fn get_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<Option<ConsumerGroupMember>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_MEMBER_COLUMNS} FROM consumer_group_consumers \
             WHERE consumer_group_id = ? AND consumer_id = ?"
        ))
        .bind(consumer_group_id)
        .bind(consumer_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;
        row.as_ref().map(map_consumer_group_member).transpose()
    }

    pub async fn remove_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<()> {
        let result = sqlx::query(
            "DELETE FROM consumer_group_consumers \
             WHERE consumer_group_id = ? AND consumer_id = ?",
        )
        .bind(consumer_group_id)
        .bind(consumer_id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;
        affected(result.rows_affected(), "consumer group member")
    }

    // Plugin instances

    pub async fn list_plugin_instances(&self) -> Result<Vec<PluginInstance>> {
        let rows = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugin_instances ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugins"))?;
        rows.iter().map(map_plugin).collect()
    }

    pub async fn get_plugin_instance(&self, id: &str) -> Result<Option<PluginInstance>> {
        let row = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugin_instances WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;
        row.as_ref().map(map_plugin).transpose()
    }

    pub async fn create_plugin_instance(
        &self,
        input: &CreatePluginInstance,
    ) -> Result<PluginInstance> {
        for (table, id, entity) in [
            ("services", input.service_id.as_deref(), "service"),
            ("routes", input.route_id.as_deref(), "route"),
            ("consumers", input.consumer_id.as_deref(), "consumer"),
            (
                "consumer_groups",
                input.consumer_group_id.as_deref(),
                "consumer group",
            ),
        ] {
            if let Some(id) = id {
                self.ensure_parent(table, id, entity).await?;
            }
        }

        let id = input.id.clone().unwrap_or_else(|| new_id("plg"));
        let protocols = input
            .protocols
            .clone()
            .unwrap_or_else(|| vec!["http".into(), "https".into()]);
        sqlx::query(&format!(
            "INSERT INTO plugin_instances (id, name, instance_name, config, enabled, protocols, \
             ordering, service_id, route_id, consumer_id, consumer_group_id, tags, created_at, \
             updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&input.instance_name)
        .bind(encode_json(
            input.config.as_ref().unwrap_or(&serde_json::json!({})),
        )?)
        .bind(input.enabled.unwrap_or(true))
        .bind(encode_json(&protocols)?)
        .bind(input.ordering.as_ref().map(encode_json).transpose()?)
        .bind(&input.service_id)
        .bind(&input.route_id)
        .bind(&input.consumer_id)
        .bind(&input.consumer_group_id)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;

        self.get_plugin_instance(&id)
            .await?
            .ok_or_else(|| Error::Storage("plugin disappeared after insert".into()))
    }

    pub async fn update_plugin_instance(
        &self,
        id: &str,
        input: &UpdatePluginInstance,
    ) -> Result<PluginInstance> {
        let service_set = input.service_id.is_some();
        let route_set = input.route_id.is_some();
        let consumer_set = input.consumer_id.is_some();
        let consumer_group_set = input.consumer_group_id.is_some();

        let result = sqlx::query(&format!(
            "UPDATE plugin_instances SET \
                name = COALESCE(?, name), \
                instance_name = COALESCE(?, instance_name), \
                config = COALESCE(?, config), \
                enabled = COALESCE(?, enabled), \
                protocols = COALESCE(?, protocols), \
                ordering = COALESCE(?, ordering), \
                service_id = CASE WHEN ? THEN ? ELSE service_id END, \
                route_id = CASE WHEN ? THEN ? ELSE route_id END, \
                consumer_id = CASE WHEN ? THEN ? ELSE consumer_id END, \
                consumer_group_id = CASE WHEN ? THEN ? ELSE consumer_group_id END, \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(&input.instance_name)
        .bind(input.config.as_ref().map(encode_json).transpose()?)
        .bind(input.enabled)
        .bind(input.protocols.as_ref().map(encode_json).transpose()?)
        .bind(input.ordering.as_ref().map(encode_json).transpose()?)
        .bind(service_set)
        .bind(input.service_id.clone().flatten())
        .bind(route_set)
        .bind(input.route_id.clone().flatten())
        .bind(consumer_set)
        .bind(input.consumer_id.clone().flatten())
        .bind(consumer_group_set)
        .bind(input.consumer_group_id.clone().flatten())
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;

        affected(result.rows_affected(), "plugin")?;
        self.get_plugin_instance(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: plugin not found".into()))
    }

    pub async fn delete_plugin_instance(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM plugin_instances WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "plugin"))?;
        affected(result.rows_affected(), "plugin")
    }

    // Key-auth credentials

    pub async fn list_credentials(
        &self,
        consumer_id: Option<&str>,
    ) -> Result<Vec<KeyauthCredential>> {
        let rows = sqlx::query(&format!(
            "SELECT {KEYAUTH_COLUMNS} FROM keyauth_credentials \
             WHERE (?1 IS NULL OR consumer_id = ?1) ORDER BY created_at ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter().map(map_keyauth).collect()
    }

    pub async fn get_credential(&self, id: &str) -> Result<Option<KeyauthCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {KEYAUTH_COLUMNS} FROM keyauth_credentials WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref().map(map_keyauth).transpose()
    }

    pub async fn create_credential(
        &self,
        input: &CreateKeyauthCredential,
    ) -> Result<KeyauthCredential> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("cred"));
        let key = input
            .key
            .clone()
            .unwrap_or_else(|| format!("key_{}", uuid::Uuid::new_v4().simple()));
        sqlx::query(&format!(
            "INSERT INTO keyauth_credentials (id, consumer_id, key, ttl, tags, created_at) \
             VALUES (?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&key)
        .bind(input.ttl)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
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
        input: &UpdateKeyauthCredential,
    ) -> Result<KeyauthCredential> {
        let result = sqlx::query(&format!(
            "UPDATE keyauth_credentials SET \
                key = COALESCE(?, key), \
                ttl = COALESCE(?, ttl), \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        ))
        .bind(&input.key)
        .bind(input.ttl)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
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
        let result = sqlx::query("DELETE FROM keyauth_credentials WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    // Basic-auth credentials

    pub async fn list_basicauth(&self) -> Result<Vec<BasicauthCredential>> {
        let rows = sqlx::query(&format!(
            "SELECT {BASICAUTH_COLUMNS} FROM basicauth_credentials ORDER BY username ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter().map(map_basicauth).collect()
    }

    pub async fn create_basicauth(
        &self,
        input: &CreateBasicauthCredential,
    ) -> Result<BasicauthCredential> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("basic"));
        sqlx::query(&format!(
            "INSERT INTO basicauth_credentials (id, consumer_id, username, password, tags, \
             created_at) VALUES (?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.username)
        .bind(&input.password)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        self.get_basicauth(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn get_basicauth(&self, id: &str) -> Result<Option<BasicauthCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {BASICAUTH_COLUMNS} FROM basicauth_credentials WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref().map(map_basicauth).transpose()
    }

    pub async fn update_basicauth(
        &self,
        id: &str,
        input: &UpdateBasicauthCredential,
    ) -> Result<BasicauthCredential> {
        let result = sqlx::query(&format!(
            "UPDATE basicauth_credentials SET \
                username = COALESCE(?, username), \
                password = COALESCE(?, password), \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        ))
        .bind(&input.username)
        .bind(&input.password)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_basicauth(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_basicauth(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM basicauth_credentials WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    // JWT secrets

    pub async fn list_jwt(&self) -> Result<Vec<JwtSecret>> {
        let rows = sqlx::query(&format!(
            "SELECT {JWT_COLUMNS} FROM jwt_secrets ORDER BY created_at ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter().map(map_jwt).collect()
    }

    pub async fn create_jwt(&self, input: &CreateJwtSecret) -> Result<JwtSecret> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("jwt"));
        sqlx::query(&format!(
            "INSERT INTO jwt_secrets (id, consumer_id, key, secret, algorithm, rsa_public_key, \
             tags, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.key)
        .bind(&input.secret)
        .bind(input.algorithm.clone().unwrap_or_else(|| "HS256".into()))
        .bind(&input.rsa_public_key)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        self.get_jwt(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn get_jwt(&self, id: &str) -> Result<Option<JwtSecret>> {
        let row = sqlx::query(&format!(
            "SELECT {JWT_COLUMNS} FROM jwt_secrets WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref().map(map_jwt).transpose()
    }

    pub async fn update_jwt(&self, id: &str, input: &UpdateJwtSecret) -> Result<JwtSecret> {
        let rsa_set = input.rsa_public_key.is_some();
        let result = sqlx::query(&format!(
            "UPDATE jwt_secrets SET \
                key = COALESCE(?, key), \
                secret = COALESCE(?, secret), \
                algorithm = COALESCE(?, algorithm), \
                rsa_public_key = CASE WHEN ? THEN ? ELSE rsa_public_key END, \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        ))
        .bind(&input.key)
        .bind(&input.secret)
        .bind(&input.algorithm)
        .bind(rsa_set)
        .bind(input.rsa_public_key.clone().flatten())
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_jwt(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_jwt(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM jwt_secrets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    // ACL entries

    pub async fn list_acls(&self, consumer_id: Option<&str>) -> Result<Vec<Acl>> {
        let rows = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM acls \
             WHERE (?1 IS NULL OR consumer_id = ?1) ORDER BY \"group\" ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acls"))?;
        rows.iter().map(map_acl).collect()
    }

    pub async fn create_acl(&self, input: &CreateAcl) -> Result<Acl> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("acl"));
        sqlx::query(&format!(
            "INSERT INTO acls (id, consumer_id, \"group\", tags, created_at) \
             VALUES (?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.group)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acl"))?;
        self.get_acl(&id)
            .await?
            .ok_or_else(|| Error::Storage("acl disappeared after insert".into()))
    }

    pub async fn get_acl(&self, id: &str) -> Result<Option<Acl>> {
        let row = sqlx::query(&format!("SELECT {ACL_COLUMNS} FROM acls WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_db_error(e, "acl"))?;
        row.as_ref().map(map_acl).transpose()
    }

    pub async fn update_acl(&self, id: &str, input: &UpdateAcl) -> Result<Acl> {
        let result = sqlx::query(&format!(
            "UPDATE acls SET \"group\" = COALESCE(?, \"group\"), tags = COALESCE(?, tags) \
             WHERE id = ?",
        ))
        .bind(&input.group)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acl"))?;
        affected(result.rows_affected(), "acl")?;
        self.get_acl(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: acl not found".into()))
    }

    pub async fn delete_acl(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM acls WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "acl"))?;
        affected(result.rows_affected(), "acl")
    }

    /// Loads every entity the API data plane needs in a constant number of
    /// queries (no per-service or per-upstream N+1 reads).
    pub async fn load_runtime_data(&self) -> Result<ApiRuntimeData> {
        let mut tx = self.begin().await?;

        let services =
            fetch_rows::<Service>(&mut tx, "services", SERVICE_COLUMNS, map_service).await?;
        let routes = fetch_rows::<Route>(&mut tx, "routes", ROUTE_COLUMNS, map_route).await?;
        let upstreams =
            fetch_rows::<Upstream>(&mut tx, "upstreams", UPSTREAM_COLUMNS, map_upstream).await?;
        let targets = fetch_rows::<Target>(&mut tx, "targets", TARGET_COLUMNS, map_target).await?;
        let consumers =
            fetch_rows::<Consumer>(&mut tx, "consumers", CONSUMER_COLUMNS, map_consumer).await?;
        let consumer_groups = fetch_rows::<ConsumerGroup>(
            &mut tx,
            "consumer_groups",
            CONSUMER_GROUP_COLUMNS,
            map_consumer_group,
        )
        .await?;
        let member_rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_MEMBER_COLUMNS} FROM consumer_group_consumers \
             ORDER BY consumer_group_id ASC, consumer_id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "consumer group members"))?;
        let consumer_group_members = member_rows
            .iter()
            .map(map_consumer_group_member)
            .collect::<Result<Vec<_>>>()?;
        let keyauth = fetch_rows::<KeyauthCredential>(
            &mut tx,
            "keyauth_credentials",
            KEYAUTH_COLUMNS,
            map_keyauth,
        )
        .await?;
        let basicauth = fetch_rows::<BasicauthCredential>(
            &mut tx,
            "basicauth_credentials",
            BASICAUTH_COLUMNS,
            map_basicauth,
        )
        .await?;
        let jwt = fetch_rows::<JwtSecret>(&mut tx, "jwt_secrets", JWT_COLUMNS, map_jwt).await?;
        let acls = fetch_rows::<Acl>(&mut tx, "acls", ACL_COLUMNS, map_acl).await?;

        let counts = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM services) AS services,
                (SELECT COUNT(*) FROM routes) AS routes,
                (SELECT COUNT(*) FROM upstreams) AS upstreams,
                (SELECT COUNT(*) FROM targets) AS targets,
                (SELECT COUNT(*) FROM consumers) AS consumers,
                (SELECT COUNT(*) FROM consumer_groups) AS consumer_groups,
                (SELECT COUNT(*) FROM keyauth_credentials) AS credentials,
                (SELECT COUNT(*) FROM plugin_instances) AS plugins",
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
            consumers,
            consumer_groups,
            consumer_group_members,
            keyauth,
            basicauth,
            jwt,
            acls,
            counts: ApiRuntimeCounts {
                services: decode(counts.try_get("services"), "counts")?,
                routes: decode(counts.try_get("routes"), "counts")?,
                upstreams: decode(counts.try_get("upstreams"), "counts")?,
                targets: decode(counts.try_get("targets"), "counts")?,
                consumers: decode(counts.try_get("consumers"), "counts")?,
                credentials: decode(counts.try_get("credentials"), "counts")?,
                plugins: decode(counts.try_get("plugins"), "counts")?,
            },
        })
    }

    pub async fn refresh_runtime_data(&self) -> Result<ApiRuntimeData> {
        self.load_runtime_data().await
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

async fn fetch_rows<T>(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    columns: &str,
    mapper: fn(&SqliteRow) -> Result<T>,
) -> Result<Vec<T>> {
    let rows = sqlx::query(&format!(
        "SELECT {columns} FROM {table} ORDER BY created_at ASC, id ASC"
    ))
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| map_db_error(e, table))?;
    rows.iter().map(mapper).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    async fn setup() -> Storage {
        Storage::connect_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn service_crud_and_unique_conflict() {
        let storage = setup().await;
        let service = storage
            .api
            .create_service(&CreateService {
                name: "orders".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(service.host, "orders");
        assert_eq!(service.retries, 5);

        let duplicate = storage
            .api
            .create_service(&CreateService {
                id: Some("svc-2".into()),
                name: "orders".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(duplicate, Err(Error::Storage(ref msg)) if msg.contains("conflict")));

        let updated = storage
            .api
            .update_service(
                &service.id,
                &UpdateService {
                    retries: Some(2),
                    description: Some("updated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.retries, 2);
        assert_eq!(updated.description, "updated");

        storage.api.delete_service(&service.id).await.unwrap();
        assert!(
            storage
                .api
                .get_service(&service.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            storage.api.delete_service(&service.id).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn route_filters_and_parent_checks() {
        let storage = setup().await;
        let service = storage
            .api
            .create_service(&CreateService {
                name: "svc".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let route = storage
            .api
            .create_route(&CreateRoute {
                name: Some("api".into()),
                service_id: Some(service.id.clone()),
                paths: Some(vec!["/api".into()]),
                methods: Some(vec!["GET".into()]),
                headers: Some(BTreeMap::from([("x-env".into(), vec!["prod".into()])])),
                strip_path: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(route.methods, vec!["GET".to_string()]);
        assert_eq!(route.paths, vec!["/api".to_string()]);
        assert_eq!(route.headers["x-env"], vec!["prod".to_string()]);

        assert_eq!(
            storage
                .api
                .list_routes(Some(&service.id))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .api
                .list_routes(Some("missing"))
                .await
                .unwrap()
                .len(),
            0
        );

        let missing = storage
            .api
            .create_route(&CreateRoute {
                service_id: Some("missing".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(missing, Err(Error::NotFound(_))));

        let cleared = storage
            .api
            .update_route(
                &route.id,
                &UpdateRoute {
                    methods: Some(vec![]),
                    regex_priority: Some(7),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(cleared.methods.is_empty());
        assert_eq!(cleared.regex_priority, 7);
    }

    #[tokio::test]
    async fn targets_and_runtime_loading() {
        let storage = setup().await;
        let upstream = storage
            .api
            .create_upstream(&CreateUpstream {
                name: "primary".into(),
                algorithm: Some("weighted-round-robin".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(upstream.algorithm, LoadBalanceAlgorithm::WeightedRoundRobin);

        storage
            .api
            .create_target(&CreateTarget {
                upstream_id: upstream.id.clone(),
                target: "127.0.0.1:9000".into(),
                weight: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            storage
                .api
                .list_targets(Some(&upstream.id))
                .await
                .unwrap()
                .len(),
            1
        );

        let consumer = storage
            .api
            .create_consumer(&CreateConsumer {
                username: Some("team".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let credential = storage
            .api
            .create_credential(&CreateKeyauthCredential {
                consumer_id: consumer.id.clone(),
                key: Some("ok_abcd1234".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(credential.key, "ok_abcd1234");

        let duplicate = storage
            .api
            .create_credential(&CreateKeyauthCredential {
                consumer_id: consumer.id.clone(),
                key: Some("ok_abcd1234".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(duplicate, Err(Error::Storage(ref m)) if m.contains("conflict")));

        let data = storage.api.load_runtime_data().await.unwrap();
        assert_eq!(data.upstreams.len(), 1);
        assert_eq!(data.targets.len(), 1);
        assert_eq!(data.consumers.len(), 1);
        assert_eq!(data.keyauth.len(), 1);
    }

    #[tokio::test]
    async fn consumer_groups_and_membership() {
        let storage = setup().await;
        let group = storage
            .api
            .create_consumer_group(&CreateConsumerGroup {
                name: "admins".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let consumer = storage
            .api
            .create_consumer(&CreateConsumer {
                username: Some("alice".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(
            storage
                .api
                .get_consumer_group_by_name("admins")
                .await
                .unwrap()
                .unwrap()
                .id,
            group.id
        );

        storage
            .api
            .add_consumer_group_member(&group.id, &consumer.id)
            .await
            .unwrap();
        let duplicate = storage
            .api
            .add_consumer_group_member(&group.id, &consumer.id)
            .await;
        assert!(matches!(duplicate, Err(Error::Storage(ref m)) if m.contains("conflict")));

        let members = storage
            .api
            .list_consumer_group_members(&group.id)
            .await
            .unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].username.as_deref(), Some("alice"));
        assert_eq!(
            storage
                .api
                .list_consumer_groups_for_consumer(&consumer.id)
                .await
                .unwrap()
                .len(),
            1
        );

        let plugin = storage
            .api
            .create_plugin_instance(&CreatePluginInstance {
                name: "rate-limiting".into(),
                consumer_group_id: Some(group.id.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plugin.consumer_group_id.as_deref(), Some(group.id.as_str()));

        let data = storage.api.load_runtime_data().await.unwrap();
        assert_eq!(data.consumer_groups.len(), 1);
        assert_eq!(data.consumer_group_members.len(), 1);

        storage
            .api
            .remove_consumer_group_member(&group.id, &consumer.id)
            .await
            .unwrap();
        assert!(
            storage
                .api
                .list_consumer_group_members(&group.id)
                .await
                .unwrap()
                .is_empty()
        );

        let renamed = storage
            .api
            .update_consumer_group(
                &group.id,
                &UpdateConsumerGroup {
                    name: Some("operators".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.name, "operators");

        storage.api.delete_consumer_group(&group.id).await.unwrap();
        assert!(
            storage
                .api
                .list_plugin_instances()
                .await
                .unwrap()
                .is_empty()
        );
    }
}
