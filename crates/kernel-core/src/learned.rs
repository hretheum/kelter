//! Governed, reversible self-learning (Phase 10, ADR 0016).
//!
//! The runtime distils reusable **facts** from past runs into a **distinct,
//! provenance-tagged, reversible** layer of memory. Two invariants make this
//! safe — learning can never silently corrupt behaviour:
//!
//! 1. **Reversibility (complete).** Learned facts live in their own tagged
//!    store, never merged into episodes or any persistent ranking index
//!    (ranking is recomputed per retrieval). `forget` deletes the rows, so the
//!    next run simply does not load them — zero residue.
//! 2. **Learning informs proposals, never bypasses validation.** A learned
//!    fact enters ONLY as retrieved *context* on the model's prompt (the same
//!    seam as episodic memory). The kernel still validates every proposed
//!    action against the schema — a learned fact is never an unvalidated
//!    authority, and even a poisoned one cannot trigger an unvalidated action.
//!
//! Every learned fact carries provenance: the source run id(s), when/what
//! model distilled it, `level = "ai_extracted"`, and `human_validated = false`
//! — the project's provenance convention applied to runtime memory.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::store::StoreError;

/// The provenance level for a distilled fact — extracted by the runtime from
/// run data, not authored by a human or generated as model prose.
pub const LEARNED_LEVEL: &str = "ai_extracted";

/// Provenance of a learned fact — nothing learned exists without it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearnedProvenance {
    /// The completed run(s) this fact was distilled from.
    pub source_run_ids: Vec<Uuid>,
    /// Unix seconds when the distillation ran.
    pub distilled_at: u64,
    /// The policy/model that produced the source run(s) (e.g. `local:…`).
    pub model: String,
    /// Provenance level — always `ai_extracted` for distilled facts.
    pub level: String,
    /// Learned facts are lower-trust until a human validates them.
    pub human_validated: bool,
    /// Groups all facts from one distillation call, for batch `forget`.
    pub batch_id: Uuid,
}

/// One reusable fact distilled from past runs. Retrieved as context by task
/// similarity; never executed, never an authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedFact {
    pub id: Uuid,
    /// The reusable knowledge, in one line.
    pub fact: String,
    /// What task this fact is relevant to — the text retrieval ranks against.
    pub task_context: String,
    pub provenance: LearnedProvenance,
}

impl LearnedFact {
    /// The one-line context string injected into a run's prompt. Tagged
    /// `[learned]` so it is visibly lower-trust and distinguishable from
    /// episodic memory both to the model and to a human inspector.
    pub fn as_context(&self) -> String {
        format!("[learned] {}", self.fact)
    }
}

/// One reusable plan distilled from a completed plan-mode run. Retrieved as a
/// candidate by task similarity, re-validated before it can be adopted, never
/// an authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPlan {
    pub id: Uuid,
    /// What task this plan is relevant to — the text retrieval ranks against.
    pub task_context: String,
    /// The full DAG, arguments included.
    pub plan: crate::plan::Plan,
    pub provenance: LearnedProvenance,
}

/// The distinct, reversible learned layer — facts AND plans. Kept separate
/// from the episodic `MemoryProvider` on purpose: a separate store is what
/// makes `forget` complete (no residue) and keeps learned artifacts a tagged,
/// inspectable layer rather than indistinguishable state.
#[async_trait]
pub trait LearnedStore: Send + Sync {
    async fn put(&self, fact: LearnedFact) -> Result<(), StoreError>;
    /// Facts most relevant to `task`, best first, at most `limit`.
    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<LearnedFact>, StoreError>;
    /// All learned facts (for inspection), newest first.
    async fn list(&self) -> Result<Vec<LearnedFact>, StoreError>;
    async fn put_plan(&self, plan: LearnedPlan) -> Result<(), StoreError>;
    /// Plans most relevant to `task`, best first, at most `limit`.
    async fn retrieve_plans(
        &self,
        task: &str,
        limit: usize,
    ) -> Result<Vec<LearnedPlan>, StoreError>;
    /// All learned plans (for inspection), newest first.
    async fn list_plans(&self) -> Result<Vec<LearnedPlan>, StoreError>;
    /// Delete one fact OR plan (ids never collide across the two kinds in
    /// practice, but this checks both so callers need not know which kind an
    /// id belongs to); returns whether it existed.
    async fn forget(&self, id: Uuid) -> Result<bool, StoreError>;
    /// Delete every fact AND plan from one distillation batch; returns the
    /// combined count. One reversal path covers both artifact kinds, so a
    /// single `batch_id` always undoes everything one distillation call
    /// produced.
    async fn forget_batch(&self, batch_id: Uuid) -> Result<u64, StoreError>;
}

/// A summary of one distillation call (Phase 10; plans added by the second
/// self-learning slice) — what was learned and from where. Returned by the
/// governed operation and logged; the learned artifacts themselves are the
/// durable, inspectable record.
#[derive(Debug, Clone, Serialize)]
pub struct DistillReport {
    pub batch_id: Uuid,
    pub facts_created: usize,
    pub plans_created: usize,
    pub source_run_ids: Vec<Uuid>,
}

/// Reference in-memory learned store (the durable one is `storage::Sqlite…`).
/// Retrieval is a naive word-overlap rank — the tested contract, not a quality
/// claim; the real backend reuses BM25.
#[derive(Default)]
pub struct InMemoryLearnedStore {
    inner: std::sync::Mutex<Vec<LearnedFact>>,
    plans: std::sync::Mutex<Vec<LearnedPlan>>,
}

impl InMemoryLearnedStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn overlap(a: &str, b: &str) -> usize {
    let words: std::collections::HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    b.to_lowercase()
        .split_whitespace()
        .filter(|w| words.contains(*w))
        .count()
}

#[async_trait]
impl LearnedStore for InMemoryLearnedStore {
    async fn put(&self, fact: LearnedFact) -> Result<(), StoreError> {
        self.inner.lock().unwrap().push(fact);
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<LearnedFact>, StoreError> {
        let facts = self.inner.lock().unwrap().clone();
        let mut scored: Vec<(usize, &LearnedFact)> = facts
            .iter()
            .map(|f| (overlap(task, &f.task_context), f))
            .filter(|(s, _)| *s > 0)
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, f)| f.clone())
            .collect())
    }

    async fn list(&self) -> Result<Vec<LearnedFact>, StoreError> {
        Ok(self.inner.lock().unwrap().clone())
    }

    async fn put_plan(&self, plan: LearnedPlan) -> Result<(), StoreError> {
        self.plans.lock().unwrap().push(plan);
        Ok(())
    }

    async fn retrieve_plans(
        &self,
        task: &str,
        limit: usize,
    ) -> Result<Vec<LearnedPlan>, StoreError> {
        let plans = self.plans.lock().unwrap().clone();
        let mut scored: Vec<(usize, &LearnedPlan)> = plans
            .iter()
            .map(|p| (overlap(task, &p.task_context), p))
            .filter(|(s, _)| *s > 0)
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, p)| p.clone())
            .collect())
    }

    async fn list_plans(&self) -> Result<Vec<LearnedPlan>, StoreError> {
        Ok(self.plans.lock().unwrap().clone())
    }

    async fn forget(&self, id: Uuid) -> Result<bool, StoreError> {
        let mut facts = self.inner.lock().unwrap();
        let before = facts.len();
        facts.retain(|f| f.id != id);
        let fact_removed = facts.len() < before;
        drop(facts);

        let mut plans = self.plans.lock().unwrap();
        let before = plans.len();
        plans.retain(|p| p.id != id);
        let plan_removed = plans.len() < before;

        Ok(fact_removed || plan_removed)
    }

    async fn forget_batch(&self, batch_id: Uuid) -> Result<u64, StoreError> {
        let mut facts = self.inner.lock().unwrap();
        let before = facts.len();
        facts.retain(|f| f.provenance.batch_id != batch_id);
        let facts_removed = (before - facts.len()) as u64;
        drop(facts);

        let mut plans = self.plans.lock().unwrap();
        let before = plans.len();
        plans.retain(|p| p.provenance.batch_id != batch_id);
        let plans_removed = (before - plans.len()) as u64;

        Ok(facts_removed + plans_removed)
    }
}
