use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use octra_desktop_daemon::{DaemonManager, DaemonStatus};
use serde::Deserialize;
use tauri::Manager;
use tokio::sync::Mutex;

type DaemonState = Mutex<DaemonManager>;

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

fn control_bind() -> String {
    fs::read_to_string(octra_home().join("config.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<FileConfig>(&raw).ok())
        .and_then(|config| config.daemon.and_then(|daemon| daemon.control_bind))
        .unwrap_or_else(|| "127.0.0.1:8081".to_string())
}

fn control_base_url() -> String {
    format!("http://{}", control_bind())
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

/// One authorization-code callback captured by the loopback listener.
#[derive(Debug, serde::Serialize)]
struct OAuthCallback {
    code: String,
    state: String,
}

/// Resolves a loopback redirect URI to the host and port to listen on.
fn oauth_callback_target(redirect_uri: &str) -> Result<(String, u16), String> {
    let url = reqwest::Url::parse(redirect_uri)
        .map_err(|err| format!("invalid OAuth redirect URI: {err}"))?;
    if url.scheme() != "http" {
        return Err("OAuth redirect URI must use http on a loopback host".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "OAuth redirect URI is missing a host".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return Err(format!(
            "OAuth redirect URI host must be loopback (127.0.0.1, localhost, or ::1), got {host}"
        ));
    }
    let port = url
        .port()
        .ok_or_else(|| "OAuth redirect URI must include an explicit port".to_string())?;
    Ok((host.to_string(), port))
}

fn oauth_callback_display(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn oauth_callback_page(title: &str, message: &str) -> String {
    format!(
        concat!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
            "<title>{title}</title></head>",
            "<body style=\"margin:0;min-height:100vh;display:grid;place-items:center;",
            "background:#0b0d10;color:#e6e8eb;font-family:system-ui,sans-serif\">",
            "<main style=\"max-width:28rem;padding:2rem;text-align:center\">",
            "<h1 style=\"margin:0 0 0.5rem;font-size:1.1rem;font-weight:600\">{title}</h1>",
            "<p style=\"margin:0;font-size:0.9rem;color:#9aa4b2\">{message}</p>",
            "</main></body></html>"
        ),
        title = title,
        message = message
    )
}

fn write_oauth_callback_response(stream: &mut TcpStream, status: &str, page: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Reads the request target from the callback connection's request line.
fn read_oauth_callback_target(stream: &mut TcpStream) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| format!("cannot configure the OAuth callback connection: {err}"))?;

    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];
    while !buffer.contains(&b'\n') && buffer.len() < 8 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(format!("cannot read the OAuth callback request: {err}")),
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let request_line = text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    parts.next();
    let target = parts
        .next()
        .ok_or_else(|| "OAuth callback request was malformed".to_string())?;
    Ok(target.to_string())
}

fn oauth_callback_params(target: &str) -> Result<HashMap<String, String>, String> {
    let url = reqwest::Url::parse(target)
        .or_else(|_| reqwest::Url::parse(&format!("http://127.0.0.1{target}")))
        .map_err(|err| format!("OAuth callback request target is invalid: {err}"))?;
    Ok(url.query_pairs().into_owned().collect())
}

fn handle_oauth_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<OAuthCallback, String> {
    stream
        .set_nonblocking(false)
        .map_err(|err| format!("cannot configure the OAuth callback connection: {err}"))?;

    let target = match read_oauth_callback_target(stream) {
        Ok(target) => target,
        Err(err) => {
            let page = oauth_callback_page(
                "Octra authorization failed",
                "The OAuth callback request could not be read. You can close this window and paste the authorization code manually.",
            );
            write_oauth_callback_response(stream, "400 Bad Request", &page);
            return Err(err);
        }
    };
    let params = match oauth_callback_params(&target) {
        Ok(params) => params,
        Err(err) => {
            let page = oauth_callback_page(
                "Octra authorization failed",
                "The OAuth callback request was malformed. You can close this window and paste the authorization code manually.",
            );
            write_oauth_callback_response(stream, "400 Bad Request", &page);
            return Err(err);
        }
    };

    if let Some(error) = params.get("error").filter(|value| !value.is_empty()) {
        let page = oauth_callback_page(
            "Octra authorization failed",
            "The provider reported an authorization error. You can close this window and paste the authorization code manually.",
        );
        write_oauth_callback_response(stream, "400 Bad Request", &page);
        return Err(params
            .get("error_description")
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| error.clone()));
    }

    let Some(code) = params
        .get("code")
        .filter(|value| !value.is_empty())
        .cloned()
    else {
        let page = oauth_callback_page(
            "Octra authorization failed",
            "The OAuth callback did not include an authorization code. You can close this window and paste the code manually.",
        );
        write_oauth_callback_response(stream, "400 Bad Request", &page);
        return Err("OAuth callback did not include a code".to_string());
    };

    if params.get("state").map(String::as_str) != Some(expected_state) {
        let page = oauth_callback_page(
            "Octra authorization failed",
            "The OAuth callback state did not match this authorization request. You can close this window and paste the authorization code manually.",
        );
        write_oauth_callback_response(stream, "400 Bad Request", &page);
        return Err("OAuth callback state mismatch".to_string());
    }

    let page = oauth_callback_page(
        "Octra authorization complete",
        "Octra authorization complete. You can close this window.",
    );
    write_oauth_callback_response(stream, "200 OK", &page);
    Ok(OAuthCallback {
        code,
        state: expected_state.to_string(),
    })
}

fn wait_for_oauth_callback(
    host: &str,
    port: u16,
    expected_state: &str,
    timeout_secs: u64,
) -> Result<OAuthCallback, String> {
    let display = oauth_callback_display(host, port);
    let listener = TcpListener::bind((host, port)).map_err(|err| {
        format!(
            "cannot bind the OAuth callback listener on {display}: {err}; paste the authorization code manually instead"
        )
    })?;
    listener.set_nonblocking(true).map_err(|err| {
        format!("cannot configure the OAuth callback listener on {display}: {err}")
    })?;

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                return handle_oauth_callback_connection(&mut stream, expected_state);
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out after {timeout_secs}s waiting for the OAuth callback on {display}; paste the authorization code manually instead"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                return Err(format!(
                    "OAuth callback listener on {display} failed: {err}; paste the authorization code manually instead"
                ));
            }
        }
    }
}

/// Waits for one OAuth authorization-code callback on a loopback redirect URI.
///
/// The listener binds the redirect URI host and port, validates the returned
/// `state`, and returns the code without logging or persisting it. The webview
/// falls back to manual code entry when the listener cannot start or times out.
#[tauri::command]
async fn oauth_wait_callback(
    redirect_uri: String,
    state: String,
    timeout_secs: Option<u64>,
) -> Result<OAuthCallback, String> {
    let (host, port) = oauth_callback_target(&redirect_uri)?;
    let timeout_secs = timeout_secs.unwrap_or(180).clamp(5, 600);
    tauri::async_runtime::spawn_blocking(move || {
        wait_for_oauth_callback(&host, port, &state, timeout_secs)
    })
    .await
    .map_err(|err| format!("OAuth callback task failed: {err}"))?
}

/// Reports the lifecycle state of the local daemon.
#[tauri::command]
async fn daemon_status(state: tauri::State<'_, DaemonState>) -> Result<DaemonStatus, String> {
    let mut manager = state.lock().await;
    Ok(manager.status())
}

/// Starts the daemon when no healthy instance is already running.
#[tauri::command]
async fn daemon_start(state: tauri::State<'_, DaemonState>) -> Result<DaemonStatus, String> {
    let mut manager = state.lock().await;
    manager.start().await
}

/// Stops a daemon child owned by this desktop host.
#[tauri::command]
async fn daemon_stop(state: tauri::State<'_, DaemonState>) -> Result<DaemonStatus, String> {
    let mut manager = state.lock().await;
    manager.stop().await
}

/// Restarts the daemon child owned by this desktop host.
#[tauri::command]
async fn daemon_restart(state: tauri::State<'_, DaemonState>) -> Result<DaemonStatus, String> {
    let mut manager = state.lock().await;
    manager.restart().await
}

pub fn run() {
    let manager = DaemonManager::new(octra_home(), control_bind());
    let app = tauri::Builder::default()
        .manage(Mutex::new(manager))
        .invoke_handler(tauri::generate_handler![
            daemon_request,
            oauth_wait_callback,
            daemon_status,
            daemon_start,
            daemon_stop,
            daemon_restart
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Octra desktop app");

    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let state = app_handle.state::<DaemonState>();
            tauri::async_runtime::block_on(async {
                let mut manager = state.lock().await;
                let _ = manager.stop().await;
            });
        }
    });
}
