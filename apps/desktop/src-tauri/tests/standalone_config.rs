//! The gap this closes: in standalone the desktop served via
//! `api::serve_listener` → `router(config = None)`, so `/api/v1/config/model*`
//! returned 501. This test replicates exactly what `start_standalone` now
//! builds — a `SwappableOracle` + a `model_config::DaemonModelConfig` controller
//! served through `api::build_app(kernel, principals, Some(controller))` — and
//! proves the config surface works end-to-end (200, not 501), with the key
//! stored 0600, the SSRF allowlist enforced, a live model hot-swap, and the
//! overlay surviving a "restart".

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use kernel_core::{Kernel, KernelConfig, RunStore};
use serde_json::json;
use storage::{Db, SqliteMemoryProvider, SqliteRunStore};

const DUMMY_KEY: &str = "sk-or-DUMMY-standalone-test-000000000000";

/// Mirror of `start_standalone`'s wiring, minus Tauri — config dir = `dir`.
async fn serve(dir: &std::path::Path) -> (String, String) {
    let config_path = dir.join("config.toml").to_string_lossy().into_owned();
    let mut policy_cfg = policy::PolicyConfig {
        engine: "local:qwen2.5-0.5b-instruct".into(),
        base_url: None,
        latitude: None,
        max_tokens: None,
        thinking: None,
        step_result_limit: None,
        api_key: None,
    };
    model_config::apply_overlay(&config_path, &mut policy_cfg);

    let (oracle, latitude) = policy::build_oracle(&policy_cfg).unwrap();
    let swappable = Arc::new(policy::SwappableOracle::new(oracle));

    let workspace = dir.join("work");
    let tools = mcp::McpToolSource::connect_in_process(&workspace.to_string_lossy())
        .await
        .unwrap();
    let db = dir.join("kelter.db");
    let store: Arc<dyn RunStore> = Arc::new(SqliteRunStore::new(Db::open(&db).unwrap()));
    let memory = Arc::new(SqliteMemoryProvider::new(Db::open(&db).unwrap()));
    let kernel = Arc::new(Kernel::new(
        swappable.clone(),
        Arc::new(tools),
        memory,
        store,
        KernelConfig {
            latitude,
            ..KernelConfig::default()
        },
    ));

    let controller: Arc<dyn api::ModelConfigController> =
        Arc::new(model_config::DaemonModelConfig::new(
            &config_path,
            swappable,
            policy::PolicyConfig {
                api_key: None,
                ..policy_cfg.clone()
            },
            Vec::new(),
        ));
    let token = "desktop-standalone-token".to_string();
    let router = api::build_app(
        kernel,
        api::Principals::single(token.clone()),
        Some(controller),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), token)
}

#[tokio::test]
async fn standalone_model_config_works_end_to_end_no_501() {
    let dir = std::env::temp_dir().join(format!("kelter-standalone-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let (base, token) = serve(&dir).await;
    let c = reqwest::Client::new();

    // 1. GET /config/model → 200 (not 501), shows the current engine.
    let r = c
        .get(format!("{base}/api/v1/config/model"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "config surface must be 200 in standalone");
    let view: serde_json::Value = r.json().await.unwrap();
    assert_eq!(view["engine"], "local:qwen2.5-0.5b-instruct");
    assert_eq!(view["api_key_set"], false);

    // 2. POST /config/model/key → 204; secret stored 0600; never echoed.
    let r = c
        .post(format!("{base}/api/v1/config/model/key"))
        .bearer_auth(&token)
        .json(&json!({ "api_key": DUMMY_KEY }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let keyfile = dir.join("kelter-secret-openrouter.key");
    assert!(keyfile.is_file(), "secret file not written");
    let mode = std::fs::metadata(&keyfile).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "secret file perms {mode:o}, expected 600");
    assert!(std::fs::read_to_string(&keyfile)
        .unwrap()
        .contains(DUMMY_KEY));
    // GET now reports the key is set — but never returns the value.
    let view: serde_json::Value = c
        .get(format!("{base}/api/v1/config/model"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["api_key_set"], true);
    assert!(!serde_json::to_string(&view).unwrap().contains(DUMMY_KEY));

    // 3. Test connection: SSRF allowlist — a metadata IP is refused with a real
    //    detail (not 501).
    let r: serde_json::Value = c
        .post(format!("{base}/api/v1/config/model/test"))
        .bearer_auth(&token)
        .json(&json!({ "provider": "local", "base_url": "http://169.254.169.254" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["ok"], false);
    assert!(r["detail"].as_str().unwrap().contains("allowlist"), "{r}");

    // 4. Hot-swap: switch to a frontier model (the key we stored makes the
    //    openrouter oracle buildable), then a NEW run's policy is the new model
    //    with no restart.
    let r = c
        .post(format!("{base}/api/v1/config/model"))
        .bearer_auth(&token)
        .json(&json!({
            "engine": "openrouter:anthropic/claude-sonnet-5",
            "base_url": "https://openrouter.ai/api/v1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "set model must succeed with a key present");
    let run_id = c
        .post(format!("{base}/api/v1/runs"))
        .bearer_auth(&token)
        .json(&json!({ "task": "anything" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run: serde_json::Value = c
        .get(format!("{base}/api/v1/runs/{run_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        run["policy"], "openrouter:anthropic/claude-sonnet-5",
        "the swapped oracle must drive the next run (hot-swap)"
    );

    // 5. Restart-survival: the overlay was persisted and wins over the default
    //    Settings.engine on the next standalone start.
    assert!(
        dir.join("model-override.toml").is_file(),
        "overlay not persisted"
    );
    let mut fresh = policy::PolicyConfig {
        engine: "local:qwen2.5-0.5b-instruct".into(),
        base_url: None,
        latitude: None,
        max_tokens: None,
        thinking: None,
        step_result_limit: None,
        api_key: None,
    };
    model_config::apply_overlay(&dir.join("config.toml").to_string_lossy(), &mut fresh);
    assert_eq!(
        fresh.engine, "openrouter:anthropic/claude-sonnet-5",
        "on restart the UI-chosen model must win over the default"
    );

    eprintln!("standalone config OK: 200 (not 501) · key 0600 · SSRF allowlist · hot-swap · restart-survival");
}
