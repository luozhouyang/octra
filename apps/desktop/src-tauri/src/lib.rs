use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct FileConfig {
    daemon: Option<DaemonSection>,
}

#[derive(Debug, Deserialize)]
struct DaemonSection {
    control_bind: Option<String>,
}

fn octra_home() -> PathBuf {
    std::env::var_os("OCTRA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".octra")))
        .unwrap_or_else(|| PathBuf::from(".octra"))
}

fn control_base_url() -> String {
    let bind = fs::read_to_string(octra_home().join("config.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<FileConfig>(&raw).ok())
        .and_then(|config| config.daemon.and_then(|daemon| daemon.control_bind))
        .unwrap_or_else(|| "127.0.0.1:8081".to_string());

    format!("http://{bind}")
}

fn control_token() -> Result<String, String> {
    if let Ok(token) = std::env::var("OCTRA_CONTROL_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    fs::read_to_string(octra_home().join("control.token"))
        .map(|token| token.trim().to_string())
        .map_err(|_| "control token not found; start the daemon with `pnpm daemon`".to_string())
}

#[tauri::command]
async fn daemon_health() -> Result<serde_json::Value, String> {
    let token = control_token()?;
    let response = reqwest::Client::new()
        .get(format!("{}/api/v1/system/health", control_base_url()))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| format!("cannot reach the Octra daemon: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("daemon returned {}", response.status()));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("invalid daemon response: {err}"))
}

#[tauri::command]
async fn daemon_pii_models() -> Result<serde_json::Value, String> {
    let token = control_token()?;
    let response = reqwest::Client::new()
        .get(format!("{}/admin/llm/pii-models", control_base_url()))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| format!("cannot reach the Octra daemon: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("daemon returned {}", response.status()));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("invalid daemon response: {err}"))
}

#[tauri::command]
async fn daemon_pii_action(
    id: String,
    action: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let token = control_token()?;
    let response = reqwest::Client::new()
        .post(format!("{}/admin/llm/pii-models/{id}", control_base_url()))
        .bearer_auth(token)
        .json(&action)
        .send()
        .await
        .map_err(|err| format!("cannot reach the Octra daemon: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("daemon returned {}", response.status()));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("invalid daemon response: {err}"))
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            daemon_health,
            daemon_pii_models,
            daemon_pii_action
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Octra desktop app");
}
