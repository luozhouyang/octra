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
        Command::Services => print_json(control.get("/api/v1/services")?)?,
        Command::Routes => print_json(control.get("/api/v1/routes")?)?,
        Command::Reload => print_json(control.post("/api/v1/system/reload")?)?,
        Command::Doctor => doctor(&control)?,
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
}

fn parse_response(response: reqwest::blocking::Response) -> Result<serde_json::Value> {
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .unwrap_or_else(|_| serde_json::json!({ "error": "response was not valid JSON" }));

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
