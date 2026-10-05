//! ADR 0036 on the real SQLite backend: `rejected` and `resumed` journal
//! entries are written to and read back from an on-disk database, across a
//! simulated process restart (the database file is closed and reopened by a
//! fresh kernel), and the resume stays exactly-once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use kernel_core::journal::{RejectionSource, RejectionSubject, ResumeCause};
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunStatus, RunStore, ValidatedAction, IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;
use storage::{Db, SqliteRunStore};

struct Script(Mutex<Vec<ProposedAction>>, AtomicUsize);

#[async_trait]
impl PolicyOracle for Script {
    async fn propose(
        &self,
        _c: &PolicyContext,
        _s: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        let mut s = self.0.lock().await;
        if s.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(s.remove(0).into())
    }
    fn describe(&self) -> String {
        "mock:script".into()
    }
}

fn script(v: Vec<(&str, Value)>) -> Arc<Script> {
    Arc::new(Script(
        Mutex::new(
            v.into_iter()
                .map(|(a, args)| ProposedAction {
                    action: a.into(),
                    arguments: args,
                    rationale: None,
                })
                .collect(),
        ),
        AtomicUsize::new(0),
    ))
}

#[derive(Default)]
struct Record(Mutex<Vec<Value>>);

#[async_trait]
impl ActionSource for Record {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "record".into(),
                description: "Record (side effect)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "entry": {"type": "string"},
                        IDEMPOTENCY_KEY_ARG: {"type": "string"}
                    },
                    "required": ["entry"],
                    "additionalProperties": false
                }),
            }],
        })
    }
    async fn execute(
        &self,
        a: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<Value, ExecutionError> {
        self.0.lock().await.push(a.arguments().clone());
        Ok(json!({"recorded": true}))
    }
}

fn db_path() -> PathBuf {
    std::env::temp_dir().join(format!("kelter-adr0036-{}.db", uuid::Uuid::new_v4()))
}

fn kernel(
    path: &PathBuf,
    oracle: Arc<Script>,
    tools: Arc<Record>,
) -> (Kernel, Arc<SqliteRunStore>) {
    let store = Arc::new(SqliteRunStore::new(Db::open(path).unwrap()));
    let k = Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig {
            latitude: Latitude::Standard,
            ..KernelConfig::default()
        },
    );
    (k, store)
}

/// The raw `kind` of every stored journal row, read with a plain SQLite
/// connection of our own — what any other reader of the file sees.
fn raw_kinds(path: &PathBuf, run_id: uuid::Uuid) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT entry FROM journal WHERE run_id = ?1 ORDER BY id ASC")
        .unwrap();
    stmt.query_map([run_id.to_string()], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|raw| {
            serde_json::from_str::<Value>(&raw.unwrap()).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn rejections_persist_in_sqlite_and_read_back_typed() {
    let path = db_path();
    let tools = Arc::new(Record::default());
    let (k, store) = kernel(
        &path,
        script(vec![
            ("record", json!({"wrong": true})),
            ("record", json!({"entry": "x"})),
            ("record", json!({"entry": "x"})),
            ("finish", json!({"summary": "done"})),
        ]),
        tools.clone(),
    );
    let run_id = k.create_run("record".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    drop((k, store));

    // A fresh connection to the same file reads both refusals back typed.
    let store = SqliteRunStore::new(Db::open(&path).unwrap());
    let journal = store.journal(run_id).await.unwrap();
    let rej: Vec<_> = journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Rejected {
                step,
                source,
                subject,
                arguments,
                ..
            } => Some((*step, *source, *subject, arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        rej,
        vec![
            (
                0,
                RejectionSource::Schema,
                RejectionSubject::Proposal,
                Some(json!({"wrong": true}))
            ),
            (
                1,
                RejectionSource::Duplicate,
                RejectionSubject::Proposal,
                Some(json!({"entry": "x"}))
            ),
        ]
    );
    let kinds = raw_kinds(&path, run_id);
    assert_eq!(
        kinds.iter().filter(|k| *k == "rejected").count(),
        2,
        "{kinds:?}"
    );

    // Retention (ADR 0017) prunes a terminal run's journal by run id — the
    // informational rows go with it, no special case.
    let report = store
        .prune(&kernel_core::RetentionPolicy {
            max_runs: Some(0),
            max_age_secs: None,
        })
        .await
        .unwrap();
    assert_eq!(report.runs_pruned, 1);
    assert_eq!(report.journal_entries_pruned, kinds.len());
    assert!(raw_kinds(&path, run_id).is_empty());
    let _ = std::fs::remove_file(&path);
}

/// Killed after the side effect; a NEW kernel on the reopened file resumes:
/// no re-execution, one `resumed` row, decision 0 marked replayed.
#[tokio::test]
async fn a_resume_across_a_reopened_sqlite_file_is_journaled_and_exactly_once() {
    let path = db_path();
    let first_tools = Arc::new(Record::default());
    let (k, store) = kernel(&path, script(vec![]), first_tools);
    let run_id = k.create_run("record".into()).await.unwrap();
    for e in [
        JournalEntry::Decision {
            step: 0,
            action: "record".into(),
            arguments: json!({"entry": "x"}),
            usage: None,
        },
        JournalEntry::Intent {
            step: 0,
            action: "record".into(),
            arguments: json!({"entry": "x"}),
            idempotency_key: format!("{run_id}:0"),
        },
        JournalEntry::Receipt {
            step: 0,
            ok: true,
            result: json!({"recorded": true}),
        },
    ] {
        store.append_journal(run_id, e).await.unwrap();
    }
    drop((k, store)); // "crash"

    let tools = Arc::new(Record::default());
    let oracle = script(vec![("finish", json!({"summary": "done"}))]);
    let (k, store) = kernel(&path, oracle.clone(), tools.clone());
    assert_eq!(store.unfinished_runs().await.unwrap(), vec![run_id]);
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(
        tools.0.lock().await.is_empty(),
        "the side effect is not repeated"
    );
    assert_eq!(
        oracle.1.load(Ordering::SeqCst),
        1,
        "only `finish` was asked"
    );

    let journal = store.journal(run_id).await.unwrap();
    let resumed: Vec<_> = journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Resumed {
                at_step,
                cause,
                replayed_steps,
                session,
                ..
            } => Some((*at_step, *cause, replayed_steps.clone(), session.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(resumed.len(), 1);
    assert_eq!(
        (resumed[0].0, resumed[0].1, resumed[0].2.clone()),
        (0, ResumeCause::Interrupted, vec![0])
    );
    assert_eq!(
        resumed[0].3,
        kernel_core::journal::execution_session_id().to_string()
    );
    drop((k, store));
    assert!(raw_kinds(&path, run_id).contains(&"resumed".to_string()));
    let _ = std::fs::remove_file(&path);
}
