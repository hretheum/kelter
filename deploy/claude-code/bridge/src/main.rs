//! stdio MCP server for the `claude-code` Kelter extension.
//!
//! Kelter spawns this as an extension tool server (trust
//! `subprocess-unsandboxed`, ADR 0012). It speaks the stdio MCP subset Kelter
//! uses — `initialize`, `tools/list`, `tools/call`, `ping` — and forwards each
//! tool call as `POST <base_url><path>` to the Claude Code runner, which does
//! the actual work outside the Kelter container. Tools are declared in
//! `bridge.json` next to the binary, so the gate-relevant facts (which tools
//! exist, which carry `idempotency_key`, which arguments are fixed) are
//! reviewable data, not code.
//!
//! A runner answer with `"ok": false` (or any HTTP error) becomes an MCP
//! result with `isError: true`, so the kernel records a failed step: it never
//! counts as the run's effective action (ADR 0033) and never satisfies an
//! `action_succeeded` contract (ADR 0037).

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
struct Tool {
    name: String,
    description: String,
    input_schema: Value,
    path: String,
    /// Arguments the bridge sets itself and the model cannot override
    /// (e.g. `model: "opus"` on the tool the approval policy gates).
    fixed: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    token: String,
    tools: Vec<Tool>,
    /// How long a call may take, queueing in the runner included.
    max_wait_s: u64,
}

fn load_config(path: &Path) -> Result<Config, String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .ok_or(format!("bridge.json: '{k}' missing"))
    };
    let token_path = dir.join(field("token_file")?);
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("{}: {e}", token_path.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("{}: empty token", token_path.display()));
    }
    let mut tools = Vec::new();
    for t in v
        .get("tools")
        .and_then(Value::as_array)
        .ok_or("bridge.json: 'tools' missing")?
    {
        let s = |k: &str| {
            t.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or(format!("bridge.json: tool '{k}' missing"))
        };
        let input_schema = t
            .get("inputSchema")
            .cloned()
            .ok_or("bridge.json: tool 'inputSchema' missing")?;
        let fixed = t
            .get("fixed")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // A fixed argument must not also be offered to the model.
        for k in fixed.keys() {
            if input_schema.pointer(&format!("/properties/{k}")).is_some() {
                return Err(format!(
                    "bridge.json: tool '{}' both fixes and exposes '{k}'",
                    s("name")?
                ));
            }
        }
        tools.push(Tool {
            name: s("name")?,
            description: s("description")?,
            input_schema,
            path: s("path")?,
            fixed,
        });
    }
    Ok(Config {
        base_url: field("base_url")?.trim_end_matches('/').to_string(),
        token,
        tools,
        max_wait_s: v.get("max_wait_s").and_then(Value::as_u64).unwrap_or(21600),
    })
}

/// What a forwarded call returned: HTTP status + JSON body, or a transport
/// error. Abstracted so the protocol logic is testable without a runner.
type Forward<'a> = dyn Fn(&str, &Value, Duration) -> Result<(u16, Value), String> + Sync + 'a;

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn call_tool(cfg: &Config, params: &Value, forward: &Forward) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(tool) = cfg.tools.iter().find(|t| t.name == name) else {
        return tool_result(format!("unknown tool '{name}'"), true);
    };
    let mut args = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (k, v) in &tool.fixed {
        args.insert(k.clone(), v.clone());
    }
    // The runner bounds each task itself and runs one at a time, so a call
    // may first wait in its queue: give up only after `max_wait_s`, never
    // after this task's own limit (a premature give-up would look like a
    // failure and invite a needless escalation).
    let wait = cfg.max_wait_s;
    let url = format!("{}{}", cfg.base_url, tool.path);
    match forward(&url, &Value::Object(args), Duration::from_secs(wait)) {
        Ok((200, body)) => {
            let ok = body.get("ok").and_then(Value::as_bool) == Some(true);
            tool_result(serde_json::to_string_pretty(&body).unwrap_or_default(), !ok)
        }
        Ok((status, body)) => tool_result(
            format!(
                "runner HTTP {status}: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("")
            ),
            true,
        ),
        Err(e) => tool_result(format!("runner unreachable: {e}"), true),
    }
}

/// Handle one JSON-RPC message; `None` for notifications.
fn handle(cfg: &Config, msg: &Value, forward: &Forward) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-03-26")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "claude-code-bridge", "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": cfg.tools.iter().map(|t| json!({
                "name": t.name, "description": t.description, "inputSchema": t.input_schema,
            })).collect::<Vec<_>>()
        }),
        "tools/call" => call_tool(cfg, &params, forward),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }))
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn http_forward(token: String) -> impl Fn(&str, &Value, Duration) -> Result<(u16, Value), String> {
    move |url, body, timeout| {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(url)
            .header("Authorization", &format!("Bearer {token}"))
            .send_json(body)
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let value = resp.body_mut().read_json::<Value>().unwrap_or(Value::Null);
        Ok((status, value))
    }
}

fn config_path() -> PathBuf {
    if let Some(p) = std::env::args().nth(1) {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("bridge.json")))
        .unwrap_or_else(|| PathBuf::from("bridge.json"))
}

fn main() {
    let cfg = match load_config(&config_path()) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("claude-code-bridge: {e}");
            std::process::exit(2);
        }
    };
    let forward = Arc::new(http_forward(cfg.token.clone()));
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let emit = |out: &Mutex<std::io::Stdout>, v: Value| {
        let mut o = out.lock().expect("stdout lock");
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            emit(
                &out,
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }),
            );
            continue;
        };
        // A tool call can run for hours; answer everything else meanwhile.
        if msg.get("method").and_then(Value::as_str) == Some("tools/call") {
            let (cfg, forward, out) = (cfg.clone(), forward.clone(), out.clone());
            std::thread::spawn(move || {
                if let Some(r) = handle(&cfg, &msg, &*forward) {
                    emit(&out, r);
                }
            });
        } else if let Some(r) = handle(&cfg, &msg, &*forward) {
            emit(&out, r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        // One directory per call: tests run in parallel.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ccb-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "sekret\n").unwrap();
        let shipped = include_str!("../../../extensions/claude-code/bridge.json");
        let mut v: Value = serde_json::from_str(shipped).unwrap();
        v["token_file"] = json!("token");
        std::fs::write(dir.join("bridge.json"), v.to_string()).unwrap();
        load_config(&dir.join("bridge.json")).unwrap()
    }

    fn call(name: &str, args: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": args } })
    }

    #[test]
    fn shipped_config_loads_and_token_is_trimmed() {
        let c = cfg();
        assert_eq!(c.token, "sekret");
        let names: Vec<_> = c.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["claude_code_run", "claude_code_opus", "sprawdz"]);
    }

    #[test]
    fn run_tools_are_mutating_and_check_is_not() {
        // ADR 0031/0038: `idempotency_key` in the schema is what makes the
        // approval gate see a tool at all.
        let c = cfg();
        let has_key = |n: &str| {
            c.tools
                .iter()
                .find(|t| t.name == n)
                .unwrap()
                .input_schema
                .pointer("/properties/idempotency_key")
                .is_some()
        };
        assert!(has_key("claude_code_run"));
        assert!(has_key("claude_code_opus"));
        assert!(!has_key("sprawdz"));
    }

    #[test]
    fn opus_is_fixed_by_the_bridge_and_cheap_tool_cannot_ask_for_it() {
        let c = cfg();
        let opus = c
            .tools
            .iter()
            .find(|t| t.name == "claude_code_opus")
            .unwrap();
        assert_eq!(opus.fixed.get("model"), Some(&json!("opus")));
        let run = c
            .tools
            .iter()
            .find(|t| t.name == "claude_code_run")
            .unwrap();
        let models = run.input_schema.pointer("/properties/model/enum").unwrap();
        assert_eq!(models, &json!(["haiku", "sonnet"]));

        let seen = Mutex::new(Value::Null);
        let fwd = |_: &str, b: &Value, _: Duration| {
            *seen.lock().unwrap() = b.clone();
            Ok((200, json!({ "ok": true })))
        };
        // Even a smuggled `model` argument is overwritten on the opus tool.
        handle(
            &c,
            &call(
                "claude_code_opus",
                json!({ "model": "haiku", "zlecenie": "x" }),
            ),
            &fwd,
        );
        assert_eq!(seen.lock().unwrap()["model"], json!("opus"));
    }

    #[test]
    fn runner_ok_false_and_http_errors_are_tool_errors() {
        let c = cfg();
        let r = |status: u16, body: Value| {
            let fwd = move |_: &str, _: &Value, _: Duration| Ok((status, body.clone()));
            handle(&c, &call("sprawdz", json!({})), &fwd).unwrap()["result"]["isError"].clone()
        };
        assert_eq!(r(200, json!({ "ok": true })), json!(false));
        assert_eq!(r(200, json!({ "ok": false })), json!(true));
        assert_eq!(r(200, json!({})), json!(true));
        assert_eq!(r(400, json!({ "error": "bad" })), json!(true));
        let down = |_: &str, _: &Value, _: Duration| Err("connection refused".to_string());
        let res = handle(&c, &call("sprawdz", json!({})), &down).unwrap();
        assert_eq!(res["result"]["isError"], json!(true));
    }

    #[test]
    fn wait_covers_the_queue_not_just_this_task() {
        let c = cfg();
        let got = Mutex::new(Duration::ZERO);
        let fwd = |_: &str, _: &Value, t: Duration| {
            *got.lock().unwrap() = t;
            Ok((200, json!({ "ok": true })))
        };
        handle(
            &c,
            &call("claude_code_run", json!({ "limit_czasu_s": 600 })),
            &fwd,
        );
        assert_eq!(*got.lock().unwrap(), Duration::from_secs(21600));
    }

    #[test]
    fn protocol_basics() {
        let c = cfg();
        let never =
            |_: &str, _: &Value, _: Duration| -> Result<(u16, Value), String> { unreachable!() };
        let init = handle(&c, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }), &never).unwrap();
        assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"));
        assert!(handle(
            &c,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            &never
        )
        .is_none());
        let list = handle(
            &c,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            &never,
        )
        .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
        let unknown = handle(
            &c,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }),
            &never,
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], json!(-32601));
        let bad = handle(&c, &call("rm_rf", json!({})), &never).unwrap();
        assert_eq!(bad["result"]["isError"], json!(true));
    }

    #[test]
    fn a_tool_that_fixes_and_exposes_the_same_field_is_refused() {
        let dir = std::env::temp_dir().join(format!("ccb-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t"), "x").unwrap();
        std::fs::write(
            dir.join("bridge.json"),
            json!({ "base_url": "http://r", "token_file": "t", "tools": [{
                "name": "a", "description": "d", "path": "/v1/run",
                "inputSchema": { "type": "object", "properties": { "model": { "type": "string" } } },
                "fixed": { "model": "opus" } }] })
            .to_string(),
        )
        .unwrap();
        assert!(load_config(&dir.join("bridge.json"))
            .unwrap_err()
            .contains("both fixes and exposes"));
    }
}
