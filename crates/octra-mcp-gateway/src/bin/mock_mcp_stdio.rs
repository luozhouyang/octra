//! Minimal MCP stdio server for gateway tests.
//!
//! Reads newline-delimited JSON-RPC on stdin and answers `initialize`,
//! `ping`, `tools/list`, and `tools/call`. Tools: `echo` (returns its
//! `message` argument), `fail` (returns a JSON-RPC error), and `slow`
//! (sleeps `delay_ms`, default 50, before answering). `notifications/*` are
//! ignored. Exits on EOF.
//!
//! Extra behaviour for failure-path tests:
//! - `MOCK_MCP_EXIT_TOOL=name` makes `tools/call` with that name exit(1)
//!   without answering.
//! - `MOCK_MCP_HANG_TOOL=name` makes `tools/call` with that name never answer.
//! - `MOCK_MCP_FAIL_INIT=1` answers the first `initialize` with an error.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut initialized = false;

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            // Notifications (e.g. notifications/initialized) need no answer.
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let reply = match method {
            "initialize" if std::env::var("MOCK_MCP_FAIL_INIT").is_ok() => {
                error(id, -32603, "mock initialization failure")
            }
            "initialize" => {
                initialized = true;
                success(
                    id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mock-mcp", "version": "0.1.0"}
                    }),
                )
            }
            "ping" => success(id, json!({})),
            "tools/list" => success(
                id,
                json!({
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echoes its message argument",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"]
                            }
                        },
                        {
                            "name": "fail",
                            "description": "Always fails",
                            "inputSchema": {"type": "object"}
                        },
                        {
                            "name": "slow",
                            "description": "Answers after a delay",
                            "inputSchema": {"type": "object"}
                        }
                    ]
                }),
            ),
            "tools/call" if !initialized => error(id, -32603, "not initialized"),
            "tools/call" => call(id, request.get("params")),
            other => error(id, -32601, &format!("method not found: {other}")),
        };

        if let Some(reply) = reply {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Returns `None` when the tool exits the process without answering.
fn call(id: Value, params: Option<&Value>) -> Option<Value> {
    let params = params.cloned().unwrap_or(Value::Null);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    if std::env::var("MOCK_MCP_EXIT_TOOL").is_ok_and(|value| value == name) {
        std::process::exit(1);
    }

    match name {
        "echo" => {
            let message = arguments
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            success(
                id,
                json!({
                    "content": [{"type": "text", "text": message}],
                    "isError": false
                }),
            )
        }
        "fail" => error(id, -32000, "tool failed on purpose"),
        "slow" => {
            let delay = arguments
                .get("delay_ms")
                .and_then(Value::as_u64)
                .unwrap_or(50);
            let hang = std::env::var("MOCK_MCP_HANG_TOOL").is_ok_and(|value| value == name);
            let nanos = if hang {
                u64::MAX
            } else {
                std::time::Duration::from_millis(delay).as_nanos() as u64
            };
            std::thread::sleep(std::time::Duration::from_nanos(nanos));
            success(id, json!({"content": [{"type": "text", "text": "slow"}]}))
        }
        other => error(id, -32602, &format!("unknown tool: {other}")),
    }
}

fn success(id: Value, result: Value) -> Option<Value> {
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i64, message: &str) -> Option<Value> {
    Some(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }))
}
