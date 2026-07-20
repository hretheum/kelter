//! Item A (Phase 8): the desktop standalone shell now persists to the same
//! durable SQLite store the daemon uses, so runs survive a desktop restart.
//!
//! This test replicates exactly what `start_standalone` builds — two handles
//! to one WAL database file (SqliteRunStore + SqliteMemoryProvider) — and
//! proves a run written in one "session" is still there after the store is
//! dropped ("desktop closed") and reopened at the same path ("restarted").
//! Before Phase 8 the shell used `InMemoryRunStore`, so this was impossible.

use std::collections::HashMap;

use kernel_core::{Run, RunStatus, RunStore, Usage};
use memory::{Episode, EpisodeOutcome, MemoryProvider, Provenance};
use storage::{Db, SqliteMemoryProvider, SqliteRunStore};

#[tokio::test]
async fn desktop_run_and_memory_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("kelter-desktop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("kelter.db");
    let run_id = uuid::Uuid::new_v4();

    // --- session 1: the desktop writes a run + a reflection episode ---------
    {
        let store = SqliteRunStore::new(Db::open(&db_path).unwrap());
        let memory = SqliteMemoryProvider::new(Db::open(&db_path).unwrap());
        let run = Run {
            id: run_id,
            task: "desktop durable run".into(),
            status: RunStatus::Completed,
            steps: Vec::new(),
            output: Some(serde_json::json!("done")),
            error: None,
            policy: "local:qwen2.5-0.5b-instruct".into(),
            created_at: 0,
            usage: Usage::default(),
            agent_usage: HashMap::new(),
            trust_posture: Vec::new(),
            cost_usd: None,
            active_skills: Vec::new(),
            pending_skill_activation: None,
            a2a_depth: 0,
            a2a_ancestry: Vec::new(),
        };
        store.put(run).await.unwrap();
        memory
            .write(Episode {
                id: uuid::Uuid::new_v4(),
                run_id,
                task: "desktop durable run".into(),
                trace: vec!["finish: ok".into()],
                outcome: EpisodeOutcome::Success,
                provenance: Provenance::Reflection,
                version: 1,
            })
            .await
            .unwrap();
    } // both handles dropped — "the desktop app was closed"

    // --- session 2: reopen at the same path ("desktop restarted") -----------
    let store = SqliteRunStore::new(Db::open(&db_path).unwrap());
    let got = store.get(run_id).await.unwrap();
    assert_eq!(got.task, "desktop durable run");
    assert_eq!(got.status, RunStatus::Completed);

    let memory = SqliteMemoryProvider::new(Db::open(&db_path).unwrap());
    assert_eq!(memory.episode_count().await.unwrap(), 1);

    eprintln!(
        "desktop restart-survival OK: run {run_id} ('{}') + 1 episode persisted across reopen",
        got.task
    );
}
