//! Run + journal persistence trait and the in-memory reference
//! implementation (tests, ephemeral topologies). The durable SQLite
//! implementation lives in the `storage` crate.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::engine::{Run, RunStatus};
use crate::journal::JournalEntry;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("run {0} not found")]
    NotFound(Uuid),
    #[error("store backend error: {0}")]
    Backend(String),
}

/// Data-lifecycle policy for pruning the durable store (ADR 0017, Phase 9).
/// A run is a prune candidate only if it is **terminal** AND either older than
/// `max_age_secs` or beyond the newest `max_runs`. An in-flight
/// (pending/running) run is NEVER a candidate.
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    /// Keep at most this many terminal runs (newest); prune older terminals.
    pub max_runs: Option<usize>,
    /// Prune terminal runs older than this many seconds.
    pub max_age_secs: Option<u64>,
}

impl RetentionPolicy {
    /// True when the policy would never prune anything.
    pub fn is_noop(&self) -> bool {
        self.max_runs.is_none() && self.max_age_secs.is_none()
    }

    /// Which of `terminal` (id, created_at, newest-first) to prune under this
    /// policy, given `now`. Pure so it is unit-testable in isolation.
    pub fn select(&self, terminal_newest_first: &[(Uuid, u64)], now: u64) -> Vec<Uuid> {
        let age_cutoff = self.max_age_secs.map(|s| now.saturating_sub(s));
        terminal_newest_first
            .iter()
            .enumerate()
            .filter(|(idx, (_, created))| {
                let too_old = age_cutoff.is_some_and(|c| *created < c);
                let over_count = self.max_runs.is_some_and(|n| *idx >= n);
                too_old || over_count
            })
            .map(|(_, (id, _))| *id)
            .collect()
    }
}

/// What a prune removed (Phase 9). Terminal runs only.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PruneReport {
    pub runs_pruned: usize,
    pub journal_entries_pruned: usize,
    pub episodes_pruned: usize,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[async_trait]
pub trait RunStore: Send + Sync {
    async fn put(&self, run: Run) -> Result<(), StoreError>;
    async fn get(&self, id: Uuid) -> Result<Run, StoreError>;
    async fn list(&self) -> Result<Vec<Run>, StoreError>;

    /// Append one journal entry for a run, durably, in order.
    async fn append_journal(&self, run_id: Uuid, entry: JournalEntry) -> Result<(), StoreError>;
    /// The full journal for a run, in append order.
    async fn journal(&self, run_id: Uuid) -> Result<Vec<JournalEntry>, StoreError>;
    /// Runs that never reached a terminal state (candidates for resume).
    async fn unfinished_runs(&self) -> Result<Vec<Uuid>, StoreError>;

    /// Prune TERMINAL runs (and their journal + episodes) per the policy.
    /// NEVER removes a non-terminal (in-flight) run — durability and
    /// exactly-once are untouched. A no-op policy prunes nothing (ADR 0017).
    async fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport, StoreError>;
}

#[derive(Default)]
pub struct InMemoryRunStore {
    runs: Arc<RwLock<HashMap<Uuid, Run>>>,
    journals: Arc<RwLock<HashMap<Uuid, Vec<JournalEntry>>>>,
}

impl InMemoryRunStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn put(&self, run: Run) -> Result<(), StoreError> {
        self.runs.write().await.insert(run.id, run);
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Run, StoreError> {
        self.runs
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or(StoreError::NotFound(id))
    }

    async fn list(&self) -> Result<Vec<Run>, StoreError> {
        let mut runs: Vec<Run> = self.runs.read().await.values().cloned().collect();
        runs.sort_by_key(|r| r.created_at);
        Ok(runs)
    }

    async fn append_journal(&self, run_id: Uuid, entry: JournalEntry) -> Result<(), StoreError> {
        self.journals
            .write()
            .await
            .entry(run_id)
            .or_default()
            .push(entry);
        Ok(())
    }

    async fn journal(&self, run_id: Uuid) -> Result<Vec<JournalEntry>, StoreError> {
        Ok(self
            .journals
            .read()
            .await
            .get(&run_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn unfinished_runs(&self) -> Result<Vec<Uuid>, StoreError> {
        Ok(self
            .runs
            .read()
            .await
            .values()
            .filter(|r| {
                matches!(
                    r.status,
                    RunStatus::Pending
                        | RunStatus::Running
                        | RunStatus::AwaitingSkillDecision
                        | RunStatus::AwaitingDelegationDecision
                        | RunStatus::AwaitingDelegationInput
                )
            })
            .map(|r| r.id)
            .collect())
    }

    async fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport, StoreError> {
        if policy.is_noop() {
            return Ok(PruneReport::default());
        }
        // Terminal runs only, newest first — in-flight runs are excluded here
        // and can never be selected.
        let mut terminal: Vec<(Uuid, u64)> = self
            .runs
            .read()
            .await
            .values()
            .filter(|r| {
                matches!(
                    r.status,
                    RunStatus::Completed | RunStatus::Failed | RunStatus::Canceled
                )
            })
            .map(|r| (r.id, r.created_at))
            .collect();
        terminal.sort_by_key(|(_, created)| std::cmp::Reverse(*created));

        let victims = policy.select(&terminal, now_secs());
        let mut journal_entries_pruned = 0;
        {
            let mut runs = self.runs.write().await;
            let mut journals = self.journals.write().await;
            for id in &victims {
                runs.remove(id);
                journal_entries_pruned += journals.remove(id).map(|j| j.len()).unwrap_or(0);
            }
        }
        Ok(PruneReport {
            runs_pruned: victims.len(),
            journal_entries_pruned,
            episodes_pruned: 0, // episodes live in the MemoryProvider seam here
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Usage;

    fn run(status: RunStatus, created_at: u64) -> Run {
        Run {
            id: Uuid::new_v4(),
            task: "t".into(),
            status,
            steps: Vec::new(),
            output: None,
            error: None,
            policy: "mock".into(),
            created_at,
            usage: Usage::default(),
            agent_usage: Default::default(),
            trust_posture: Vec::new(),
            cost_usd: None,
            active_skills: Vec::new(),
            pending_skill_activation: None,
            pending_delegation: None,
            pending_delegation_input: None,
            a2a_depth: 0,
            a2a_ancestry: Vec::new(),
            max_steps: None,
        }
    }

    #[test]
    fn select_prunes_only_by_count_and_age_never_negative_index() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        // newest first, created_at descending
        let terminal: Vec<(Uuid, u64)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, (100 - i as u64)))
            .collect();
        let policy = RetentionPolicy {
            max_runs: Some(2),
            max_age_secs: None,
        };
        let victims = policy.select(&terminal, 1000);
        // Keep the newest 2 (indices 0,1); prune 2,3,4.
        assert_eq!(victims, vec![ids[2], ids[3], ids[4]]);
    }

    #[tokio::test]
    async fn prune_keeps_n_terminal_and_never_touches_in_flight() {
        let store = InMemoryRunStore::new();
        // 4 terminal runs + 1 in-flight (running).
        let mut terminal_ids = Vec::new();
        for i in 0..4 {
            let r = run(RunStatus::Completed, i);
            terminal_ids.push(r.id);
            store.put(r).await.unwrap();
            store
                .append_journal(
                    *terminal_ids.last().unwrap(),
                    JournalEntry::Terminal {
                        status: RunStatus::Completed,
                        output: None,
                        error: None,
                    },
                )
                .await
                .unwrap();
        }
        let inflight = run(RunStatus::Running, 999);
        let inflight_id = inflight.id;
        store.put(inflight).await.unwrap();

        // Keep the newest 2 terminal → prune the 2 oldest terminal.
        let report = store
            .prune(&RetentionPolicy {
                max_runs: Some(2),
                max_age_secs: None,
            })
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 2);
        assert_eq!(report.journal_entries_pruned, 2);

        // The in-flight run is untouched…
        assert!(store.get(inflight_id).await.is_ok());
        // …and exactly 2 terminal + the in-flight remain (3 total).
        assert_eq!(store.list().await.unwrap().len(), 3);
        // The 2 oldest terminal are gone.
        assert!(store.get(terminal_ids[0]).await.is_err());
        assert!(store.get(terminal_ids[1]).await.is_err());
    }

    #[tokio::test]
    async fn noop_policy_prunes_nothing() {
        let store = InMemoryRunStore::new();
        store.put(run(RunStatus::Completed, 1)).await.unwrap();
        let report = store.prune(&RetentionPolicy::default()).await.unwrap();
        assert_eq!(report.runs_pruned, 0);
        assert_eq!(store.list().await.unwrap().len(), 1);
    }
}
