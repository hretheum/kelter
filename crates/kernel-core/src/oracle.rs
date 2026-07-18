//! The policy oracle seam: `propose(context, action_schema) -> proposed
//! action`. Adapters live behind this trait; the kernel never talks to a
//! model directly and never trusts what comes back (see [`crate::action`]).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::action::{ActionSchema, ProposedAction};
use crate::engine::StepRecord;

/// How much freedom the policy gets. Strong engines earn a wider action
/// space and less decomposition; weak engines get constrained harder.
/// Same kernel, different latitude per capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Latitude {
    /// Weak engine: tight schemas, step-by-step guidance injected into
    /// context, more validation retries.
    Constrained,
    #[default]
    Standard,
    /// Strong engine: full action space, minimal hand-holding.
    Wide,
}

impl Latitude {
    /// Validation/proposal retries the kernel grants per step.
    pub fn max_retries(self) -> u32 {
        match self {
            Latitude::Constrained => 4,
            Latitude::Standard => 3,
            Latitude::Wide => 2,
        }
    }

    /// Whether the kernel injects explicit decomposition guidance into the
    /// policy context.
    pub fn inject_guidance(self) -> bool {
        matches!(self, Latitude::Constrained)
    }

    /// Constrained latitude decomposes harder: actions that already
    /// succeeded are removed from the offered space, so a weak engine is
    /// physically unable to loop on one tool (the grammar excludes it).
    pub fn narrow_action_space(self) -> bool {
        matches!(self, Latitude::Constrained)
    }

    /// Outside `Wide`, an exact repeat (same action, same arguments) of an
    /// already-successful step is rejected with feedback instead of
    /// re-executed.
    pub fn reject_duplicates(self) -> bool {
        !matches!(self, Latitude::Wide)
    }
}

/// Everything the policy oracle sees at a decision point.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyContext {
    pub task: String,
    /// Prior steps in this run (action + outcome), oldest first.
    pub steps: Vec<StepRecord>,
    /// Ranked context retrieved from the memory provider.
    pub memory: Vec<String>,
    /// Feedback from a rejected proposal or failed tool call, if retrying.
    pub last_error: Option<String>,
    pub latitude: Latitude,
    /// Extra guidance injected by the kernel (populated under
    /// `Latitude::Constrained`).
    pub guidance: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("model backend error: {0}")]
    Backend(String),
    #[error("model produced unusable output: {0}")]
    Malformed(String),
}

/// A proposal plus what the model call cost. Usage is accounting metadata —
/// it never influences validation.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub action: ProposedAction,
    pub usage: Option<crate::journal::Usage>,
}

impl From<ProposedAction> for Proposal {
    fn from(action: ProposedAction) -> Self {
        Self {
            action,
            usage: None,
        }
    }
}

/// The adapter contract: return a proposal drawn from the action schema, or
/// fail loudly. Adapters may use constrained decoding (local) or native
/// tool-calling (remote) to satisfy it — the kernel re-validates either way.
#[async_trait]
pub trait PolicyOracle: Send + Sync {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError>;

    /// Human-readable adapter identity (e.g. "local:qwen2.5-0.5b").
    fn describe(&self) -> String;
}
