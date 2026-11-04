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
    /// List configured services
    Services,
    /// List configured routes
    Routes,
    /// Reload the gateway runtime snapshot from SQLite
    Reload,
    /// Print local paths and daemon reachability
    Doctor,
    /// Manage LLM provider accounts
    #[command(subcommand)]
    Accounts(AccountsCommand),
    /// Manage LLM virtual keys
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Show recent LLM usage
    Usage {
        /// Maximum number of recent records
        #[arg(long, default_value_t = 50)]
        limit: i64,
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

#[derive(Subcommand)]
enum AccountsCommand {
    /// List accounts for a provider
    List {
        /// Provider id
        provider: String,
    },
    /// Create an account for a provider
    Create {
        /// Provider id
        provider: String,
        /// Account name
        name: String,
        /// Secret reference, e.g. env:OPENAI_KEY_B
        #[arg(long)]
        secret_ref: Option<String>,
        #[arg(long, default_value_t = 1)]
        weight: i64,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// Quota limit in the configured unit
        #[arg(long)]
        quota_limit: Option<i64>,
        /// requests, tokens or cost_micros
        #[arg(long, default_value = "requests")]
        quota_unit: String,
        /// none, day or month
        #[arg(long, default_value = "none")]
        quota_period: String,
    },
    /// Update an existing account
    Update {
        /// Account id
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        secret_ref: Option<String>,
        #[arg(long)]
        weight: Option<i64>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(long)]
        quota_limit: Option<i64>,
        #[arg(long)]
        quota_unit: Option<String>,
        #[arg(long)]
        quota_period: Option<String>,
    },
    /// Delete an account
    Delete {
        /// Account id
        id: String,
    },
    /// Reset an account's quota counter and cooldown
    ResetQuota {
        /// Account id
        id: String,
    },
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
        Command::Services => print_json(control.get("/admin/services")?)?,
        Command::Routes => print_json(control.get("/admin/routes")?)?,
        Command::Reload => print_json(control.post("/api/v1/system/reload")?)?,
        Command::Doctor => doctor(&control)?,
        Command::Usage { limit } => {
            print_json(control.get(&format!("/admin/llm/usage?limit={limit}"))?)?
        }
        Command::Accounts(command) => accounts(&control, command)?,
        Command::Keys(command) => keys(&control, command)?,
    }

    Ok(())
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

fn parse_json_arg(raw: Option<String>, field: &str) -> Result<Option<serde_json::Value>> {
    raw.map(|raw| {
        serde_json::from_str(&raw).with_context(|| format!("`{field}` must be valid JSON"))
    })
    .transpose()
}

fn accounts(control: &Control, command: AccountsCommand) -> Result<()> {
    match command {
        AccountsCommand::List { provider } => {
            print_json(control.get(&format!("/admin/llm/providers/{provider}/accounts"))?)?
        }
        AccountsCommand::Create {
            provider,
            name,
            secret_ref,
            weight,
            priority,
            quota_limit,
            quota_unit,
            quota_period,
        } => {
            let mut body = serde_json::json!({
                "name": name,
                "weight": weight,
                "priority": priority,
                "quota_unit": quota_unit,
                "quota_period": quota_period,
            });
            if let Some(secret_ref) = secret_ref {
                body["secret_ref"] = serde_json::json!(secret_ref);
            }
            if let Some(quota_limit) = quota_limit {
                body["quota_limit"] = serde_json::json!(quota_limit);
            }
            print_json(
                control.post_json(&format!("/admin/llm/providers/{provider}/accounts"), &body)?,
            )?
        }
        AccountsCommand::Update {
            id,
            name,
            secret_ref,
            weight,
            priority,
            enabled,
            quota_limit,
            quota_unit,
            quota_period,
        } => {
            let mut body = serde_json::Map::new();
            for (key, value) in [
                ("name", name.map(serde_json::Value::from)),
                ("secret_ref", secret_ref.map(serde_json::Value::from)),
                ("weight", weight.map(serde_json::Value::from)),
                ("priority", priority.map(serde_json::Value::from)),
                ("enabled", enabled.map(serde_json::Value::from)),
                ("quota_limit", quota_limit.map(serde_json::Value::from)),
                ("quota_unit", quota_unit.map(serde_json::Value::from)),
                ("quota_period", quota_period.map(serde_json::Value::from)),
            ] {
                if let Some(value) = value {
                    body.insert(key.to_string(), value);
                }
            }
            print_json(control.put(
                &format!("/admin/llm/accounts/{id}"),
                &serde_json::Value::Object(body),
            )?)?
        }
        AccountsCommand::Delete { id } => {
            print_json(control.delete(&format!("/admin/llm/accounts/{id}"))?)?
        }
        AccountsCommand::ResetQuota { id } => {
            print_json(control.post(&format!("/admin/llm/accounts/{id}/quota/reset"))?)?
        }
    }
    Ok(())
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
