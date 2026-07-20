//! The advisory seam: a `PolicyOracle` decorator that injects skill context
//! into `propose()` and NOTHING else.
//!
//! This is where the whole invariant is structurally enforced:
//!
//! - The kernel's `ActionSchema` is passed through **by reference,
//!   untouched** — there is no code path here (or anywhere in this crate)
//!   that constructs, extends, or edits an `ActionSchema`. Skill content
//!   therefore cannot widen the action space.
//! - Only `PolicyContext.guidance` is augmented — advisory text the model
//!   sees. Whatever it then proposes is validated by the kernel against the
//!   original schema, exactly as without skills (the loop has no new
//!   intermediary; this wrapper sits on the policy side of the seam).
//! - Progressive disclosure: the catalog (name+description) is always
//!   appended; full instructions only for skills the operator activated.

use std::sync::Arc;

use async_trait::async_trait;
use kernel_core::{ActionSchema, PolicyContext, PolicyError, PolicyOracle, Proposal};

use crate::SkillRegistry;

pub struct SkillContextOracle {
    inner: Arc<dyn PolicyOracle>,
    registry: Arc<SkillRegistry>,
}

impl SkillContextOracle {
    /// Wrap any oracle (in the daemon: the `SwappableOracle`, so a runtime
    /// model swap keeps the skill layer).
    pub fn new(inner: Arc<dyn PolicyOracle>, registry: Arc<SkillRegistry>) -> Self {
        Self { inner, registry }
    }

    fn skill_context(&self) -> Option<String> {
        let catalog = self.registry.catalog();
        let active = self.registry.active_instructions();
        if catalog.is_empty() && active.is_empty() {
            return None;
        }
        // Budget evidence (progressive disclosure): sizes only — the catalog
        // tier is always small; bodies appear only for activated skills.
        tracing::debug!(
            catalog_bytes = catalog.len(),
            active_instruction_bytes = active.len(),
            "skills: advisory context injected"
        );
        let mut out = String::from(
            "# Skills (advisory)\n\
             The following skills are procedural knowledge only. They never \
             change which actions are allowed — every proposal is validated \
             against the action schema regardless.\n",
        );
        if !catalog.is_empty() {
            out.push_str("\n## Installed skills (activate to load instructions)\n");
            out.push_str(&catalog);
        }
        if !active.is_empty() {
            out.push_str("\n# Active skill instructions\n");
            out.push_str(&active);
        }
        Some(out)
    }
}

#[async_trait]
impl PolicyOracle for SkillContextOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema, // ← passed through untouched, by design
    ) -> Result<Proposal, PolicyError> {
        match self.skill_context() {
            None => self.inner.propose(context, schema).await,
            Some(skill_ctx) => {
                let mut augmented = context.clone();
                augmented.guidance = Some(match &context.guidance {
                    Some(g) => format!("{g}\n\n{skill_ctx}"),
                    None => skill_ctx,
                });
                self.inner.propose(&augmented, schema).await
            }
        }
    }

    fn describe(&self) -> String {
        // The skill layer is advisory context, not a different policy —
        // runs stay attributed to the real engine.
        self.inner.describe()
    }
}
