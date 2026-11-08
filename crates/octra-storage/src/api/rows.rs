//! Row mappers for API gateway entities.

use super::*;

pub(super) fn map_service(row: &SqliteRow) -> Result<Service> {
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
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "service")?,
        updated_at: decode(row.try_get("updated_at"), "service")?,
    })
}

pub(super) fn map_route(row: &SqliteRow) -> Result<Route> {
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

pub(super) fn map_upstream(row: &SqliteRow) -> Result<Upstream> {
    let algorithm: String = decode(row.try_get("algorithm"), "upstream")?;
    Ok(Upstream {
        id: decode(row.try_get("id"), "upstream")?,
        name: decode(row.try_get("name"), "upstream")?,
        algorithm: LoadBalanceAlgorithm::parse(&algorithm).ok_or_else(|| {
            Error::Storage(format!("unknown load balance algorithm '{algorithm}'"))
        })?,
        hash_on: decode(row.try_get("hash_on"), "upstream")?,
        hash_fallback: decode(row.try_get("hash_fallback"), "upstream")?,
        hash_on_header: decode(row.try_get("hash_on_header"), "upstream")?,
        hash_fallback_header: decode(row.try_get("hash_fallback_header"), "upstream")?,
        hash_on_cookie: decode(row.try_get("hash_on_cookie"), "upstream")?,
        hash_on_cookie_path: decode(row.try_get("hash_on_cookie_path"), "upstream")?,
        hash_on_query_arg: decode(row.try_get("hash_on_query_arg"), "upstream")?,
        hash_fallback_query_arg: decode(row.try_get("hash_fallback_query_arg"), "upstream")?,
        hash_on_uri_capture: decode(row.try_get("hash_on_uri_capture"), "upstream")?,
        hash_fallback_uri_capture: decode(row.try_get("hash_fallback_uri_capture"), "upstream")?,
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

pub(super) fn map_target(row: &SqliteRow) -> Result<Target> {
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

pub(super) fn map_consumer(row: &SqliteRow) -> Result<Consumer> {
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

pub(super) fn map_consumer_group(row: &SqliteRow) -> Result<ConsumerGroup> {
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

pub(super) fn map_consumer_group_member(row: &SqliteRow) -> Result<ConsumerGroupMember> {
    Ok(ConsumerGroupMember {
        consumer_id: decode(row.try_get("consumer_id"), "consumer group member")?,
        consumer_group_id: decode(row.try_get("consumer_group_id"), "consumer group member")?,
        created_at: decode(row.try_get("created_at"), "consumer group member")?,
    })
}

pub(super) fn map_plugin(row: &SqliteRow) -> Result<PluginInstance> {
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

pub(super) fn open_secret(
    secrets: &SecretBox,
    stored: &str,
    table: &str,
    id: &str,
    column: &str,
) -> Result<String> {
    if stored.is_empty() {
        return Ok(String::new());
    }
    let bytes = secrets
        .open_with_context(stored, &crate::secret_context(table, id, column))
        .map_err(|_| Error::Storage(format!("cannot decrypt {table}.{column} for row '{id}'")))?;
    String::from_utf8(bytes).map_err(|_| {
        Error::Storage(format!(
            "decrypted {table}.{column} for row '{id}' is not UTF-8"
        ))
    })
}

pub(super) fn map_keyauth(row: &SqliteRow, secrets: &SecretBox) -> Result<KeyauthCredential> {
    let id: String = decode(row.try_get("id"), "credential")?;
    let key: String = decode(row.try_get("key"), "credential")?;
    Ok(KeyauthCredential {
        key: open_secret(secrets, &key, "keyauth_credentials", &id, "key")?,
        id,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        ttl: decode(row.try_get("ttl"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

pub(super) fn map_basicauth(row: &SqliteRow, secrets: &SecretBox) -> Result<BasicauthCredential> {
    let id: String = decode(row.try_get("id"), "credential")?;
    let password: String = decode(row.try_get("password"), "credential")?;
    Ok(BasicauthCredential {
        password: open_secret(secrets, &password, "basicauth_credentials", &id, "password")?,
        id,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        username: decode(row.try_get("username"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

pub(super) fn map_jwt(row: &SqliteRow, secrets: &SecretBox) -> Result<JwtSecret> {
    let id: String = decode(row.try_get("id"), "credential")?;
    let secret: String = decode(row.try_get("secret"), "credential")?;
    Ok(JwtSecret {
        secret: open_secret(secrets, &secret, "jwt_secrets", &id, "secret")?,
        id,
        consumer_id: decode(row.try_get("consumer_id"), "credential")?,
        key: decode(row.try_get("key"), "credential")?,
        algorithm: decode(row.try_get("algorithm"), "credential")?,
        rsa_public_key: decode(row.try_get("rsa_public_key"), "credential")?,
        tags: decode_json_or_default(
            row.try_get::<Option<String>, _>("tags").ok().flatten(),
            "tags",
        )?,
        created_at: decode(row.try_get("created_at"), "credential")?,
    })
}

pub(super) fn map_acl(row: &SqliteRow) -> Result<Acl> {
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
