//! The LLM in-process upstream and the API gateway routes that front it.
//!
//! There is no separate LLM listener. The API gateway owns explicit routes for
//! the OpenAI, Anthropic, and Gemini endpoint paths; each route points at the
//! `llm-gateway` service tagged `llm:internal`. The API gateway resolves that
//! tag to the in-process LLM router.

use std::collections::HashSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response;
use octra_api_core::{
    CreateRoute, CreateService, LLM_INTERNAL_TAG, MANAGED_BY_LLM_TAG, UpdateRoute, UpdateService,
};
use octra_api_gateway::{InternalUpstream, InternalUpstreamContext};
use octra_storage::Storage;
use tower::ServiceExt;

const SERVICE_ID: &str = "svc_llm_gateway";
const SERVICE_NAME: &str = "llm-gateway";

/// Human-readable endpoint list, kept in sync with [`LLM_ROUTES`] and the LLM
/// router. These are the routes a client can call through the API gateway.
pub const LLM_ROUTE_PATHS: [&str; 7] = [
    "/v1/chat/completions",
    "/v1/responses",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/models",
    "/v1beta/models",
    "/v1beta/models/{model}:{action}",
];

struct LlmRouteSpec {
    id: &'static str,
    name: &'static str,
    /// API gateway path pattern. Exact regex paths prevent `/v1` from
    /// capturing unrelated API routes.
    pattern: &'static str,
}

const LLM_ROUTES: &[LlmRouteSpec] = &[
    LlmRouteSpec {
        id: "rt_llm_v1_chat_completions",
        name: "llm-openai-chat-completions",
        pattern: r"~^/v1/chat/completions$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1_responses",
        name: "llm-openai-responses",
        pattern: r"~^/v1/responses$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1_messages",
        name: "llm-anthropic-messages",
        pattern: r"~^/v1/messages$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1_messages_count_tokens",
        name: "llm-anthropic-count-tokens",
        pattern: r"~^/v1/messages/count_tokens$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1_models",
        name: "llm-models",
        pattern: r"~^/v1/models$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1beta_models",
        name: "llm-gemini-models",
        pattern: r"~^/v1beta/models$",
    },
    LlmRouteSpec {
        id: "rt_llm_v1beta_model_actions",
        name: "llm-gemini-model-actions",
        pattern: r"~^/v1beta/models/[^/]+:(generateContent|streamGenerateContent|countTokens)$",
    },
];

/// Ensures the `llm-gateway` service and its explicit endpoint routes exist.
///
/// This only provisions API gateway service/route entities. CORS, IP
/// restriction, and other policies remain ordinary API gateway plugin
/// instances attached to the service through the existing plugin API.
///
/// Runs before the API gateway runtime snapshot is compiled so the routes are
/// visible on the first request.
pub async fn ensure_llm_routes(storage: &Storage) -> Result<()> {
    ensure_service(storage).await?;
    for route in LLM_ROUTES {
        ensure_route(storage, route).await?;
    }
    remove_obsolete_routes(storage).await?;
    Ok(())
}

async fn ensure_service(storage: &Storage) -> Result<()> {
    let tags = service_tags();
    if let Some(existing) = storage
        .api
        .get_service(SERVICE_ID)
        .await
        .context("cannot read the LLM gateway service")?
    {
        let needs_update = !existing.enabled
            || !existing.tags.iter().any(|tag| tag == LLM_INTERNAL_TAG)
            || existing.host != "internal"
            || existing.retries != 0
            || existing.connect_timeout <= 0
            || existing.read_timeout <= 0;
        if needs_update {
            storage
                .api
                .update_service(
                    SERVICE_ID,
                    &UpdateService {
                        host: Some("internal".to_string()),
                        port: Some(80),
                        retries: Some(0),
                        connect_timeout: Some(60_000),
                        write_timeout: Some(60_000),
                        read_timeout: Some(60_000),
                        enabled: Some(true),
                        tags: Some(tags),
                        ..Default::default()
                    },
                )
                .await
                .context("cannot update the LLM gateway service")?;
        }
        return Ok(());
    }

    storage
        .api
        .create_service(&CreateService {
            id: Some(SERVICE_ID.to_string()),
            name: SERVICE_NAME.to_string(),
            protocol: Some("http".to_string()),
            host: Some("internal".to_string()),
            port: Some(80),
            path: None,
            retries: Some(0),
            connect_timeout: Some(60_000),
            write_timeout: Some(60_000),
            read_timeout: Some(60_000),
            enabled: Some(true),
            description: Some("In-process LLM gateway upstream".to_string()),
            tags: Some(tags),
        })
        .await
        .context("cannot create the LLM gateway service")?;
    tracing::info!(
        service = SERVICE_NAME,
        "provisioned the LLM gateway service"
    );
    Ok(())
}

async fn ensure_route(storage: &Storage, spec: &LlmRouteSpec) -> Result<()> {
    let tags = entity_tags();
    let protocols = vec!["http".to_string(), "https".to_string()];
    if let Some(existing) = storage
        .api
        .get_route(spec.id)
        .await
        .context("cannot read an LLM gateway route")?
    {
        let needs_update = existing.service_id.as_deref() != Some(SERVICE_ID)
            || !existing.enabled
            || existing.paths != vec![spec.pattern.to_string()]
            || !existing.methods.is_empty()
            || existing.strip_path
            || existing.protocols != protocols
            || !existing.tags.iter().any(|tag| tag == MANAGED_BY_LLM_TAG);
        if needs_update {
            storage
                .api
                .update_route(
                    spec.id,
                    &UpdateRoute {
                        paths: Some(vec![spec.pattern.to_string()]),
                        methods: Some(Vec::new()),
                        protocols: Some(protocols),
                        strip_path: Some(false),
                        service_id: Some(Some(SERVICE_ID.to_string())),
                        enabled: Some(true),
                        tags: Some(tags),
                        ..Default::default()
                    },
                )
                .await
                .context("cannot update an LLM gateway route")?;
        }
        return Ok(());
    }

    storage
        .api
        .create_route(&CreateRoute {
            id: Some(spec.id.to_string()),
            name: Some(spec.name.to_string()),
            protocols: Some(protocols),
            // An empty method list matches every method, including the CORS
            // OPTIONS preflight that must run before LLM authentication.
            methods: Some(Vec::new()),
            hosts: None,
            paths: Some(vec![spec.pattern.to_string()]),
            headers: None,
            strip_path: Some(false),
            preserve_host: Some(false),
            regex_priority: Some(0),
            service_id: Some(SERVICE_ID.to_string()),
            enabled: Some(true),
            tags: Some(tags),
        })
        .await
        .context("cannot create an LLM gateway route")?;
    tracing::info!(route = spec.id, "provisioned an LLM gateway route");
    Ok(())
}

/// Removes routes left behind by an older prefix-based provisioning. Only
/// routes tagged as managed by the LLM reconciler are touched.
async fn remove_obsolete_routes(storage: &Storage) -> Result<()> {
    let active: HashSet<&str> = LLM_ROUTES.iter().map(|route| route.id).collect();
    for route in storage
        .api
        .list_routes(Some(SERVICE_ID))
        .await
        .context("cannot list LLM gateway routes")?
    {
        if !route.tags.iter().any(|tag| tag == MANAGED_BY_LLM_TAG)
            || active.contains(route.id.as_str())
        {
            continue;
        }
        storage
            .api
            .delete_route(&route.id)
            .await
            .context("cannot remove an obsolete LLM gateway route")?;
        tracing::info!(route = %route.id, "removed an obsolete LLM gateway route");
    }
    Ok(())
}

fn service_tags() -> Vec<String> {
    let mut tags = entity_tags();
    tags.push(LLM_INTERNAL_TAG.to_string());
    tags
}

fn entity_tags() -> Vec<String> {
    vec![MANAGED_BY_LLM_TAG.to_string(), "llm".to_string()]
}

/// Serves the LLM router in-process for the API gateway.
#[derive(Clone)]
pub struct LlmInternalUpstream {
    router: Router,
}

impl LlmInternalUpstream {
    pub fn new(router: Router) -> Self {
        Self { router }
    }
}

#[async_trait]
impl InternalUpstream for LlmInternalUpstream {
    async fn call(&self, mut request: Request, context: InternalUpstreamContext) -> Response {
        request
            .extensions_mut()
            .insert(octra_data_plane::ClientIp(context.client_ip));
        request
            .extensions_mut()
            .insert(octra_data_plane::RequestId(context.request_id));
        match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::error!(%error, "llm internal upstream failed");
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"error":{"message":"llm internal upstream failed","type":"internal"}}"#,
                    ))
                    .unwrap_or_else(|_| Response::new(Body::empty()))
            }
        }
    }
}
