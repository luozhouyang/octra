use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use octra_config::Workspace;
use reqwest::blocking::Client;

#[derive(Parser)]
#[command(name = "octra", version, about = "Control the local Octra gateway")]
struct Cli {
    /// Control plane base URL, e.g. http://127.0.0.1:8081
    #[arg(long, global = true, env = "OCTRA_CONTROL_URL")]
    control_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show daemon health and runtime snapshot
    Status,
    /// Manage API Gateway services
    #[command(subcommand)]
    Services(ServicesCommand),
    /// Manage API Gateway routes
    #[command(subcommand)]
    Routes(RoutesCommand),
    /// Manage API Gateway upstreams and targets
    #[command(subcommand)]
    Upstreams(UpstreamsCommand),
    /// Reload the gateway runtime snapshot from SQLite
    Reload,
    /// Print local paths and daemon reachability
    Doctor,
    /// Manage LLM virtual keys
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Show recent LLM usage
    Usage {
        /// Maximum number of recent records
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Inspect and activate the AI-native LLM control plane
    #[command(subcommand)]
    Ai(AiCommand),
}

#[derive(Subcommand)]
enum ServicesCommand {
    /// List services
    List,
    /// Create a service from a JSON body
    Create {
        /// JSON object with the service fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a service from a JSON body
    Update {
        /// Service id
        id: String,
        /// JSON object with the service fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a service
    Delete {
        /// Service id
        id: String,
    },
}

#[derive(Subcommand)]
enum RoutesCommand {
    /// List routes
    List,
    /// Create a route from a JSON body
    Create {
        /// JSON object with the route fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a route from a JSON body
    Update {
        /// Route id
        id: String,
        /// JSON object with the route fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a route
    Delete {
        /// Route id
        id: String,
    },
}

#[derive(Subcommand)]
enum UpstreamsCommand {
    /// List upstreams
    List,
    /// Create an upstream from a JSON body
    Create {
        /// JSON object with the upstream fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update an upstream from a JSON body
    Update {
        /// Upstream id
        id: String,
        /// JSON object with the upstream fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete an upstream
    Delete {
        /// Upstream id
        id: String,
    },
    /// Manage targets for an upstream
    #[command(subcommand)]
    Targets(UpstreamTargetsCommand),
}

#[derive(Subcommand)]
enum UpstreamTargetsCommand {
    /// List targets for an upstream
    List {
        /// Upstream id
        upstream: String,
    },
    /// Create a target for an upstream from a JSON body
    Create {
        /// Upstream id
        upstream: String,
        /// JSON object with the target fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a target from a JSON body
    Update {
        /// Target id
        id: String,
        /// JSON object with the target fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a target
    Delete {
        /// Target id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiCommand {
    /// Show AI control-plane counts, last compile, and revision count
    Status,
    /// Compile and validate the current control plane
    Validate,
    /// Compile, record a revision, and activate it
    Apply,
    /// List applied revisions
    Revisions,
    /// Restore a revision snapshot
    Rollback {
        /// Revision id
        id: String,
    },
    /// Manage AI providers
    #[command(subcommand)]
    Providers(AiProvidersCommand),
    /// Manage AI provider accounts
    #[command(subcommand)]
    Accounts(AiAccountsCommand),
    /// Manage AI models
    #[command(subcommand)]
    Models(AiModelsCommand),
    /// Manage AI model targets
    #[command(subcommand)]
    Targets(AiTargetsCommand),
    /// Manage AI policies
    #[command(subcommand)]
    Policies(AiPoliciesCommand),
    /// Manage AI policy attachments
    #[command(subcommand)]
    Attachments(AiAttachmentsCommand),
}

#[derive(Subcommand)]
enum AiProvidersCommand {
    /// List AI providers
    List,
    /// Create a provider from a JSON body
    Create {
        /// JSON object with the provider fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a provider from a JSON body
    Update {
        /// Provider id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a provider
    Delete {
        /// Provider id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiAccountsCommand {
    /// List accounts for a provider
    List {
        /// Provider id
        #[arg(long)]
        provider: String,
    },
    /// Create an account for a provider from a JSON body
    Create {
        /// Provider id
        #[arg(long)]
        provider: String,
        /// JSON object with the account fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update an account from a JSON body
    Update {
        /// Account id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete an account
    Delete {
        /// Account id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiModelsCommand {
    /// List AI models
    List,
    /// Create a model from a JSON body
    Create {
        /// JSON object with the model fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a model from a JSON body
    Update {
        /// Model id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a model
    Delete {
        /// Model id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiTargetsCommand {
    /// List targets for a model
    List {
        /// Model id
        #[arg(long)]
        model: String,
    },
    /// Create a target for a model from a JSON body
    Create {
        /// Model id
        #[arg(long)]
        model: String,
        /// JSON object with the target fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a target from a JSON body
    Update {
        /// Target id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a target
    Delete {
        /// Target id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiPoliciesCommand {
    /// List AI policies
    List,
    /// Create a policy from a JSON body
    Create {
        /// JSON object with the policy fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update a policy from a JSON body
    Update {
        /// Policy id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete a policy
    Delete {
        /// Policy id
        id: String,
    },
}

#[derive(Subcommand)]
enum AiAttachmentsCommand {
    /// List attachments for a policy
    List {
        /// Policy id
        #[arg(long)]
        policy: String,
    },
    /// Create an attachment for a policy from a JSON body
    Create {
        /// Policy id
        #[arg(long)]
        policy: String,
        /// JSON object with the attachment fields, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Update an attachment from a JSON body
    Update {
        /// Attachment id
        id: String,
        /// JSON object with the fields to change, `-` for stdin, or `@path` for a file
        #[arg(long)]
        body: String,
    },
    /// Delete an attachment
    Delete {
        /// Attachment id
        id: String,
    },
}

#[derive(Subcommand)]
enum KeysCommand {
    /// List virtual keys (prefixes only; plaintext is never stored)
    List,
    /// Create a virtual key and print its plaintext once
    Create {
        #[arg(long)]
        name: String,
        /// Consumer id to bind the key to
        #[arg(long)]
        consumer: Option<String>,
        /// JSON permissions document, e.g. '{"allowed_models":["octra-gpt"]}'
        #[arg(long)]
        permissions: Option<String>,
        /// RFC 3339 expiry timestamp
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// Rotate a virtual key and print the new plaintext once
    Rotate {
        id: String,
        #[arg(long)]
        name: Option<String>,
        /// JSON permissions override; omitted copies the old key
        #[arg(long)]
        permissions: Option<String>,
        /// Seconds the old key stays valid; 0 disables it immediately
        #[arg(long)]
        grace_seconds: Option<u64>,
        /// RFC 3339 expiry for the new key
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// Disable a virtual key without deleting it
    Revoke { id: String },
    /// Delete a virtual key permanently
    Delete { id: String },
}

struct Control {
    workspace: Workspace,
    base_url: String,
    token: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let control = Control::resolve(cli.control_url)?;

    match cli.command {
        Command::Status => print_json(control.get("/api/v1/system/health")?)?,
        Command::Services(command) => services(&control, command)?,
        Command::Routes(command) => routes(&control, command)?,
        Command::Upstreams(command) => upstreams(&control, command)?,
        Command::Reload => print_json(control.post("/api/v1/system/reload")?)?,
        Command::Doctor => doctor(&control)?,
        Command::Usage { limit } => {
            print_json(control.get(&format!("/admin/llm/usage?limit={limit}"))?)?
        }
        Command::Keys(command) => keys(&control, command)?,
        Command::Ai(command) => ai(&control, command)?,
    }

    Ok(())
}

fn services(control: &Control, command: ServicesCommand) -> Result<()> {
    match command {
        ServicesCommand::List => print_json(control.get("/admin/services")?),
        ServicesCommand::Create { body } => {
            let body = parse_json_body(&body)?;
            print_json(control.post_json("/admin/services", &body)?)
        }
        ServicesCommand::Update { id, body } => {
            let body = parse_json_body(&body)?;
            print_json(control.put(&format!("/admin/services/{id}"), &body)?)
        }
        ServicesCommand::Delete { id } => {
            print_json(control.delete(&format!("/admin/services/{id}"))?)
        }
    }
}

fn routes(control: &Control, command: RoutesCommand) -> Result<()> {
    match command {
        RoutesCommand::List => print_json(control.get("/admin/routes")?),
        RoutesCommand::Create { body } => {
            let body = parse_json_body(&body)?;
            print_json(control.post_json("/admin/routes", &body)?)
        }
        RoutesCommand::Update { id, body } => {
            let body = parse_json_body(&body)?;
            print_json(control.put(&format!("/admin/routes/{id}"), &body)?)
        }
        RoutesCommand::Delete { id } => print_json(control.delete(&format!("/admin/routes/{id}"))?),
    }
}

fn upstreams(control: &Control, command: UpstreamsCommand) -> Result<()> {
    match command {
        UpstreamsCommand::List => print_json(control.get("/admin/upstreams")?),
        UpstreamsCommand::Create { body } => {
            let body = parse_json_body(&body)?;
            print_json(control.post_json("/admin/upstreams", &body)?)
        }
        UpstreamsCommand::Update { id, body } => {
            let body = parse_json_body(&body)?;
            print_json(control.put(&format!("/admin/upstreams/{id}"), &body)?)
        }
        UpstreamsCommand::Delete { id } => {
            print_json(control.delete(&format!("/admin/upstreams/{id}"))?)
        }
        UpstreamsCommand::Targets(command) => upstream_targets(control, command),
    }
}

fn upstream_targets(control: &Control, command: UpstreamTargetsCommand) -> Result<()> {
    match command {
        UpstreamTargetsCommand::List { upstream } => {
            print_json(control.get(&format!("/admin/upstreams/{upstream}/targets"))?)
        }
        UpstreamTargetsCommand::Create { upstream, body } => {
            let body = parse_json_body(&body)?;
            print_json(control.post_json(&format!("/admin/upstreams/{upstream}/targets"), &body)?)
        }
        UpstreamTargetsCommand::Update { id, body } => {
            let body = parse_json_body(&body)?;
            print_json(control.put(&format!("/admin/targets/{id}"), &body)?)
        }
        UpstreamTargetsCommand::Delete { id } => {
            print_json(control.delete(&format!("/admin/targets/{id}"))?)
        }
    }
}

fn keys(control: &Control, command: KeysCommand) -> Result<()> {
    match command {
        KeysCommand::List => print_json(control.get("/admin/llm/virtual-keys")?)?,
        KeysCommand::Create {
            name,
            consumer,
            permissions,
            expires_at,
        } => {
            let mut body = serde_json::json!({ "name": name });
            if let Some(consumer) = consumer {
                body["consumer_id"] = serde_json::json!(consumer);
            }
            if let Some(permissions) = parse_json_arg(permissions, "permissions")? {
                body["permissions"] = permissions;
            }
            if let Some(expires_at) = expires_at {
                body["expires_at"] = serde_json::json!(expires_at);
            }
            print_json(control.post_json("/admin/llm/virtual-keys", &body)?)?
        }
        KeysCommand::Rotate {
            id,
            name,
            permissions,
            grace_seconds,
            expires_at,
        } => {
            let mut body = serde_json::Map::new();
            if let Some(name) = name {
                body.insert("name".into(), serde_json::json!(name));
            }
            if let Some(permissions) = parse_json_arg(permissions, "permissions")? {
                body.insert("permissions".into(), permissions);
            }
            if let Some(grace_seconds) = grace_seconds {
                body.insert("grace_seconds".into(), serde_json::json!(grace_seconds));
            }
            if let Some(expires_at) = expires_at {
                body.insert("expires_at".into(), serde_json::json!(expires_at));
            }
            print_json(control.post_json(
                &format!("/admin/llm/virtual-keys/{id}/rotate"),
                &serde_json::Value::Object(body),
            )?)?
        }
        KeysCommand::Revoke { id } => print_json(control.put(
            &format!("/admin/llm/virtual-keys/{id}"),
            &serde_json::json!({ "enabled": false }),
        )?)?,
        KeysCommand::Delete { id } => {
            print_json(control.delete(&format!("/admin/llm/virtual-keys/{id}"))?)?
        }
    }
    Ok(())
}

fn ai(control: &Control, command: AiCommand) -> Result<()> {
    match command {
        AiCommand::Status => print_json(control.get("/api/v1/ai/status")?),
        AiCommand::Validate => print_json(control.post("/api/v1/ai/validate")?),
        AiCommand::Apply => print_json(control.post("/api/v1/ai/apply")?),
        AiCommand::Revisions => print_json(control.get("/api/v1/ai/revisions")?),
        AiCommand::Rollback { id } => {
            print_json(control.post(&format!("/api/v1/ai/revisions/{id}/rollback"))?)
        }
        AiCommand::Providers(command) => ai_providers(control, command),
        AiCommand::Accounts(command) => ai_accounts(control, command),
        AiCommand::Models(command) => ai_models(control, command),
        AiCommand::Targets(command) => ai_targets(control, command),
        AiCommand::Policies(command) => ai_policies(control, command),
        AiCommand::Attachments(command) => ai_attachments(control, command),
    }
}

fn ai_providers(control: &Control, command: AiProvidersCommand) -> Result<()> {
    match command {
        AiProvidersCommand::List => print_json(control.get("/api/v1/ai/providers")?),
        AiProvidersCommand::Create { body } => ai_create(control, "/api/v1/ai/providers", &body),
        AiProvidersCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/providers/{id}"), &body)
        }
        AiProvidersCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/providers/{id}"))?)
        }
    }
}

fn ai_accounts(control: &Control, command: AiAccountsCommand) -> Result<()> {
    match command {
        AiAccountsCommand::List { provider } => {
            print_json(control.get(&format!("/api/v1/ai/providers/{provider}/accounts"))?)
        }
        AiAccountsCommand::Create { provider, body } => ai_create(
            control,
            &format!("/api/v1/ai/providers/{provider}/accounts"),
            &body,
        ),
        AiAccountsCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/accounts/{id}"), &body)
        }
        AiAccountsCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/accounts/{id}"))?)
        }
    }
}

fn ai_models(control: &Control, command: AiModelsCommand) -> Result<()> {
    match command {
        AiModelsCommand::List => print_json(control.get("/api/v1/ai/models")?),
        AiModelsCommand::Create { body } => ai_create(control, "/api/v1/ai/models", &body),
        AiModelsCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/models/{id}"), &body)
        }
        AiModelsCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/models/{id}"))?)
        }
    }
}

fn ai_targets(control: &Control, command: AiTargetsCommand) -> Result<()> {
    match command {
        AiTargetsCommand::List { model } => {
            print_json(control.get(&format!("/api/v1/ai/models/{model}/targets"))?)
        }
        AiTargetsCommand::Create { model, body } => ai_create(
            control,
            &format!("/api/v1/ai/models/{model}/targets"),
            &body,
        ),
        AiTargetsCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/targets/{id}"), &body)
        }
        AiTargetsCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/targets/{id}"))?)
        }
    }
}

fn ai_policies(control: &Control, command: AiPoliciesCommand) -> Result<()> {
    match command {
        AiPoliciesCommand::List => print_json(control.get("/api/v1/ai/policies")?),
        AiPoliciesCommand::Create { body } => ai_create(control, "/api/v1/ai/policies", &body),
        AiPoliciesCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/policies/{id}"), &body)
        }
        AiPoliciesCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/policies/{id}"))?)
        }
    }
}

fn ai_attachments(control: &Control, command: AiAttachmentsCommand) -> Result<()> {
    match command {
        AiAttachmentsCommand::List { policy } => {
            print_json(control.get(&format!("/api/v1/ai/policies/{policy}/attachments"))?)
        }
        AiAttachmentsCommand::Create { policy, body } => ai_create(
            control,
            &format!("/api/v1/ai/policies/{policy}/attachments"),
            &body,
        ),
        AiAttachmentsCommand::Update { id, body } => {
            ai_update(control, &format!("/api/v1/ai/attachments/{id}"), &body)
        }
        AiAttachmentsCommand::Delete { id } => {
            print_json(control.delete(&format!("/api/v1/ai/attachments/{id}"))?)
        }
    }
}

fn ai_create(control: &Control, path: &str, body: &str) -> Result<()> {
    let body = parse_json_body(body)?;
    print_json(control.post_json(path, &body)?)
}

fn ai_update(control: &Control, path: &str, body: &str) -> Result<()> {
    let body = parse_json_body(body)?;
    print_json(control.put(path, &body)?)
}

fn parse_json_body(raw: &str) -> Result<serde_json::Value> {
    let text = if raw == "-" {
        read_stdin_body()?
    } else if let Some(path) = raw.strip_prefix('@') {
        if path.is_empty() {
            bail!("`--body @` requires a file path");
        }
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read `--body` file '{path}'"))?
    } else {
        raw.to_string()
    };

    if text.trim().is_empty() {
        bail!("`--body` input is empty");
    }

    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| "`--body` must be valid JSON")?;
    if !value.is_object() {
        bail!("`--body` must be a JSON object");
    }
    Ok(value)
}

fn read_stdin_body() -> Result<String> {
    use std::io::Read;

    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("cannot read `--body` from stdin")?;
    if text.trim().is_empty() {
        bail!("`--body -` requires JSON on stdin but stdin was empty");
    }
    Ok(text)
}

fn parse_json_arg(raw: Option<String>, field: &str) -> Result<Option<serde_json::Value>> {
    raw.map(|raw| {
        serde_json::from_str(&raw).with_context(|| format!("`{field}` must be valid JSON"))
    })
    .transpose()
}

impl Control {
    fn resolve(control_url: Option<String>) -> Result<Self> {
        let workspace = Workspace::resolve()?;
        let config = octra_config::load_or_create(&workspace)?;

        let base_url = control_url
            .unwrap_or_else(|| format!("http://{}", config.daemon.control_bind))
            .trim_end_matches('/')
            .to_string();

        let token = match std::env::var("OCTRA_CONTROL_TOKEN") {
            Ok(token) if !token.trim().is_empty() => token.trim().to_string(),
            _ => {
                let path = workspace.token_file();
                let token = std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "cannot read control token at {}; start the daemon first with `pnpm daemon`",
                        path.display()
                    )
                })?;
                token.trim().to_string()
            }
        };

        Ok(Self {
            workspace,
            base_url,
            token,
        })
    }

    fn client(&self) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("cannot build HTTP client")
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let response = self
            .client()?
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("cannot reach the Octra daemon at {}", self.base_url))?;

        parse_response(response)
    }

    fn post(&self, path: &str) -> Result<serde_json::Value> {
        let response = self
            .client()?
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("cannot reach the Octra daemon at {}", self.base_url))?;

        parse_response(response)
    }

    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let response = self
            .client()?
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .with_context(|| format!("cannot reach the Octra daemon at {}", self.base_url))?;

        parse_response(response)
    }

    fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let response = self
            .client()?
            .put(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .with_context(|| format!("cannot reach the Octra daemon at {}", self.base_url))?;

        parse_response(response)
    }

    fn delete(&self, path: &str) -> Result<serde_json::Value> {
        let response = self
            .client()?
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("cannot reach the Octra daemon at {}", self.base_url))?;

        parse_response(response)
    }
}

fn parse_response(response: reqwest::blocking::Response) -> Result<serde_json::Value> {
    let status = response.status();
    let text = response
        .text()
        .context("cannot read the daemon response body")?;
    let value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(
            |_| serde_json::json!({ "error": "response was not valid JSON", "body": text }),
        )
    };

    if !status.is_success() {
        bail!("daemon returned {status}: {value}");
    }

    Ok(value)
}

fn print_json(value: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn doctor(control: &Control) -> Result<()> {
    println!("home:        {}", control.workspace.home.display());
    println!("config:      {}", control.workspace.config_file().display());
    println!("token:       {}", control.workspace.token_file().display());
    println!("control url: {}", control.base_url);

    match control.get("/api/v1/system/health") {
        Ok(value) => println!(
            "health:      {} (version {})",
            value["status"].as_str().unwrap_or("unknown"),
            value["version"].as_str().unwrap_or("unknown")
        ),
        Err(error) => println!("health:      unavailable ({error})"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_body_accepts_inline_object() {
        let body = parse_json_body(r#"{"name":"mock"}"#).unwrap();
        assert_eq!(body["name"], "mock");
    }

    #[test]
    fn parse_json_body_rejects_non_object_json() {
        let error = parse_json_body("[1,2,3]").unwrap_err().to_string();
        assert!(error.contains("JSON object"));
    }

    #[test]
    fn parse_json_body_reads_file_reference() {
        let path = std::env::temp_dir().join(format!(
            "octra-cli-body-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, r#"{"name":"from-file"}"#).unwrap();
        let body = parse_json_body(&format!("@{}", path.display())).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(body["name"], "from-file");
    }

    #[test]
    fn parse_json_body_rejects_unreadable_file() {
        let error = parse_json_body("@/definitely/not/a/real/octra-body.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot read `--body` file"));
    }
}
