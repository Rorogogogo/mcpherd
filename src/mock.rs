//! A tiny MCP server for tests (`mcpherd mock-server`, hidden). Its one tool reports the
//! server's pid and how many calls it has handled, which shows whether sessions share it.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

pub fn run() {
    let mut calls = 0u64;
    let stdout = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(method), Some(id)) = (msg["method"].as_str(), msg.get("id")) else {
            continue;
        };
        let mut out = stdout.lock();
        let result = match method {
            "initialize" => json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "1"}
            }),
            "tools/list" => {
                json!({"tools": [{"name": "whoami", "inputSchema": {"type": "object"}}]})
            }
            "tools/call" => {
                calls += 1;
                if let Some(token) = msg.pointer("/params/_meta/progressToken") {
                    let note = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                        "params": {"progressToken": token, "progress": 1, "total": 1}});
                    let _ = writeln!(out, "{note}");
                }
                json!({"content": [{"type": "text",
                    "text": format!("pid={} calls={calls}", std::process::id())}]})
            }
            _ => {
                let err = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}});
                let _ = writeln!(out, "{err}");
                continue;
            }
        };
        let _ = writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        );
        let _ = out.flush();
    }
}
