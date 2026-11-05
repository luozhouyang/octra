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

#[derive(Debug, serde::Serialize)]
struct DaemonResponse {
    status: u16,
    body: serde_json::Value,
}

/// Forwards one control-plane request through the desktop host.
///
/// The webview does not have the control token, so every page goes through
/// this command. Only loopback control-plane paths are accepted; the base URL
/// still comes from `$OCTRA_HOME/config.toml`.
#[tauri::command]
async fn daemon_request(
    method: String,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<DaemonResponse, String> {
    let path_without_query = path.split('?').next().unwrap_or(&path);
    if !(path_without_query.starts_with("/api/v1/") || path_without_query.starts_with("/admin/")) {
        return Err(format!("unsupported control path: {path}"));
    }
    let method = match method.to_ascii_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        other => return Err(format!("unsupported method: {other}")),
    };

    let token = control_token()?;
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", control_base_url()))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = request
        .send()
        .await
        .map_err(|err| format!("cannot reach the Octra daemon: {err}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|err| format!("cannot read daemon response: {err}"))?;
    let body = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
    };

    Ok(DaemonResponse { status, body })
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![daemon_request])
        .run(tauri::generate_context!())
        .expect("error while running the Octra desktop app");
}
