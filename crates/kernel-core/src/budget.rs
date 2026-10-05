//! Cost budgets (ADR 0039, KR1.3): a hard per-run ceiling on dollars, tokens
//! and wall-clock time, plus a per-principal daily dollar ceiling — enforced
//! BEFORE they are crossed. The kernel asks the policy adapter for the
//! worst case of the next model call ([`crate::oracle::CallBound`]) and
//! refuses the call when that worst case no longer fits; it re-checks before
//! every action. A refusal is journaled (`rejected`, source `budget`) and the
//! run ends `failed` with a structured [`BudgetStop`] on the record.
//!
//! The default is inert: no `[budget]`, no override → no limit, no check.

use serde::{Deserialize, Serialize};

use crate::cost::PricingTable;
use crate::journal::Usage;

/// Per-run limits. Every field optional; `None` = that dimension unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimits {
    /// Dollars of model usage under the price table. Inactive for a model
    /// the table does not price (see [`RunBudget::usd_enforced`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    /// Prompt + completion tokens over all of the run's model calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Seconds of wall-clock time from creation, minus time spent paused on
    /// a human decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_secs: Option<u64>,
}

impl BudgetLimits {
    pub fn is_empty(&self) -> bool {
        self.max_usd.is_none() && self.max_tokens.is_none() && self.max_wall_secs.is_none()
    }

    /// Apply a caller's per-run override on top of the configured limits.
    /// A configured limit is a ceiling: the override may lower it, never
    /// raise it; a dimension the config leaves open may be set to anything
    /// positive. Errors name the offending field.
    pub fn with_override(self, over: &BudgetLimits) -> Result<BudgetLimits, String> {
        fn pick<T: PartialOrd + Copy + std::fmt::Display>(
            name: &str,
            ceiling: Option<T>,
            requested: Option<T>,
            positive: impl Fn(T) -> bool,
        ) -> Result<Option<T>, String> {
            match (ceiling, requested) {
                (c, None) => Ok(c),
                (_, Some(r)) if !positive(r) => Err(format!("budget.{name} must be > 0")),
                (Some(c), Some(r)) if r > c => Err(format!(
                    "budget.{name} = {r} exceeds the configured ceiling {c}"
                )),
                (_, Some(r)) => Ok(Some(r)),
            }
        }
        Ok(BudgetLimits {
            max_usd: pick("max_usd", self.max_usd, over.max_usd, |v: f64| {
                v.is_finite() && v > 0.0
            })?,
            max_tokens: pick("max_tokens", self.max_tokens, over.max_tokens, |v| v > 0)?,
            max_wall_secs: pick(
                "max_wall_secs",
                self.max_wall_secs,
                over.max_wall_secs,
                |v| v > 0,
            )?,
        })
    }
}

/// The kernel-wide budget configuration (`[budget]`).
#[derive(Debug, Clone, Default)]
pub struct BudgetConfig {
    /// Default AND ceiling for every run's limits.
    pub per_run: BudgetLimits,
    /// Dollars a single principal's runs may spend per UTC day.
    pub daily_usd_per_principal: Option<f64>,
    /// Tokens added to every call bound for what an adapter cannot see (a
    /// provider's chat template, a hosted tool-use preamble).
    pub prompt_overhead_tokens: u64,
    /// Completion bound used when the adapter does not report its own
    /// `max_tokens`.
    pub fallback_completion_tokens: u64,
}

impl BudgetConfig {
    pub const DEFAULT_PROMPT_OVERHEAD_TOKENS: u64 = 1024;
    pub const DEFAULT_FALLBACK_COMPLETION_TOKENS: u64 = 4096;

    pub fn is_inert(&self) -> bool {
        self.per_run.is_empty() && self.daily_usd_per_principal.is_none()
    }
}

/// A run's effective limits, fixed at creation and carried on the record.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunBudget {
    #[serde(flatten)]
    pub limits: BudgetLimits,
    /// False when the run's model has no price in the table: `max_usd` (and
    /// the daily ceiling) cannot be evaluated for it and are not enforced;
    /// `max_tokens` and `max_wall_secs` still are.
    pub usd_enforced: bool,
}

/// Which limit stopped a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLimitKind {
    Usd,
    Tokens,
    WallTime,
    DailyUsd,
}

/// Where the check that stopped it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCheckpoint {
    /// Before a model call — `requested` is the call's worst case.
    ModelCall,
    /// Before an action dispatch — `requested` is 0.
    Action,
}

/// Why a run was stopped by its budget: the limit, what was already used,
/// and what the refused call would have needed at worst. Units: dollars for
/// `usd`/`daily_usd`, tokens for `tokens`, seconds for `wall_time`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetStop {
    pub limit: BudgetLimitKind,
    pub max: f64,
    pub used: f64,
    pub requested: f64,
    pub checkpoint: BudgetCheckpoint,
    pub step: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl BudgetStop {
    pub fn reason(&self) -> String {
        let (what, unit) = match self.limit {
            BudgetLimitKind::Usd => ("dollar budget", "$"),
            BudgetLimitKind::Tokens => ("token budget", " tokens"),
            BudgetLimitKind::WallTime => ("wall-clock budget", "s"),
            BudgetLimitKind::DailyUsd => ("principal's daily dollar budget", "$"),
        };
        let fmt = |v: f64| match self.limit {
            BudgetLimitKind::Usd | BudgetLimitKind::DailyUsd => format!("{unit}{v:.6}"),
            _ => format!("{v:.0}{unit}"),
        };
        let at = match self.checkpoint {
            BudgetCheckpoint::ModelCall => "before a model call",
            BudgetCheckpoint::Action => "before an action",
        };
        if self.requested > 0.0 {
            format!(
                "{what} exceeded {at} at step {}: used {} + worst case {} > max {} — stopped \
                 before crossing it",
                self.step,
                fmt(self.used),
                fmt(self.requested),
                fmt(self.max)
            )
        } else {
            format!(
                "{what} exhausted {at} at step {}: used {} of max {}",
                self.step,
                fmt(self.used),
                fmt(self.max)
            )
        }
    }
}

/// Everything one check needs, gathered by the engine.
pub struct BudgetProbe<'a> {
    pub budget: Option<&'a RunBudget>,
    pub pricing: &'a PricingTable,
    pub model: &'a str,
    /// This run's usage so far, plus tokens reserved for model calls already
    /// admitted but not yet returned (plan-mode reviews in flight).
    pub used: Usage,
    /// Worst case of the next call; `None` for an action checkpoint.
    pub next_call: Option<Usage>,
    /// Effective elapsed seconds (wall-clock minus human pauses).
    pub elapsed_secs: u64,
    /// The daily principal ceiling and what the principal's OTHER runs
    /// already spent today; `None` when no daily limit applies.
    pub daily: Option<(f64, f64)>,
    pub step: u32,
    pub branch: Option<String>,
}

/// The pure budget predicate. `None` = within budget.
pub fn check(p: &BudgetProbe<'_>) -> Option<BudgetStop> {
    let checkpoint = if p.next_call.is_some() {
        BudgetCheckpoint::ModelCall
    } else {
        BudgetCheckpoint::Action
    };
    let next = p.next_call.unwrap_or_default();
    let stop = |limit, max: f64, used: f64, requested: f64| BudgetStop {
        limit,
        max,
        used,
        requested,
        checkpoint,
        step: p.step,
        branch: p.branch.clone(),
    };
    let tokens = |u: &Usage| u.prompt_tokens + u.completion_tokens;

    if let Some(b) = p.budget {
        if let Some(max) = b.limits.max_wall_secs {
            // Wall time cannot be reserved ahead; any further work past the
            // limit is refused.
            if p.elapsed_secs >= max {
                return Some(stop(
                    BudgetLimitKind::WallTime,
                    max as f64,
                    p.elapsed_secs as f64,
                    0.0,
                ));
            }
        }
        if let Some(max) = b.limits.max_tokens {
            let (used, requested) = (tokens(&p.used), tokens(&next));
            if used.saturating_add(requested) > max {
                return Some(stop(
                    BudgetLimitKind::Tokens,
                    max as f64,
                    used as f64,
                    requested as f64,
                ));
            }
        }
        if let (Some(max), true) = (b.limits.max_usd, b.usd_enforced) {
            if let (Some(used), Some(requested)) = (
                p.pricing.cost(p.model, &p.used),
                p.pricing.cost(p.model, &next),
            ) {
                if used + requested > max {
                    return Some(stop(BudgetLimitKind::Usd, max, used, requested));
                }
            }
        }
    }
    if let Some((max, others)) = p.daily {
        if let (Some(used), Some(requested)) = (
            p.pricing.cost(p.model, &p.used),
            p.pricing.cost(p.model, &next),
        ) {
            if others + used + requested > max {
                return Some(stop(
                    BudgetLimitKind::DailyUsd,
                    max,
                    others + used,
                    requested,
                ));
            }
        }
    }
    None
}

/// Start of the UTC day containing `secs` (Unix seconds).
pub fn utc_day_start(secs: u64) -> u64 {
    secs - secs % 86_400
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::ModelPrice;
    use std::collections::HashMap;

    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            calls: 1,
        }
    }

    fn priced() -> PricingTable {
        PricingTable::new(HashMap::from([(
            "m".to_string(),
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
            },
        )]))
    }

    fn probe<'a>(b: &'a RunBudget, pricing: &'a PricingTable, used: Usage) -> BudgetProbe<'a> {
        BudgetProbe {
            budget: Some(b),
            pricing,
            model: "m",
            used,
            next_call: Some(usage(100, 100)),
            elapsed_secs: 0,
            daily: None,
            step: 3,
            branch: None,
        }
    }

    #[test]
    fn override_may_only_lower_a_configured_ceiling() {
        let cfg = BudgetLimits {
            max_usd: Some(1.0),
            max_tokens: None,
            max_wall_secs: Some(60),
        };
        let lowered = cfg
            .with_override(&BudgetLimits {
                max_usd: Some(0.5),
                max_tokens: Some(1000),
                max_wall_secs: None,
            })
            .unwrap();
        assert_eq!(lowered.max_usd, Some(0.5));
        assert_eq!(lowered.max_tokens, Some(1000));
        assert_eq!(lowered.max_wall_secs, Some(60));
        let err = cfg
            .with_override(&BudgetLimits {
                max_usd: Some(2.0),
                ..BudgetLimits::default()
            })
            .unwrap_err();
        assert!(err.contains("max_usd") && err.contains("ceiling"), "{err}");
        assert!(cfg
            .with_override(&BudgetLimits {
                max_tokens: Some(0),
                ..BudgetLimits::default()
            })
            .is_err());
    }

    #[test]
    fn tokens_stop_before_the_call_that_would_cross() {
        let b = RunBudget {
            limits: BudgetLimits {
                max_tokens: Some(1000),
                ..BudgetLimits::default()
            },
            usd_enforced: false,
        };
        let t = PricingTable::default();
        assert!(check(&probe(&b, &t, usage(400, 400))).is_none()); // 800 + 200 = 1000
        let s = check(&probe(&b, &t, usage(401, 400))).unwrap();
        assert_eq!(s.limit, BudgetLimitKind::Tokens);
        assert_eq!(s.checkpoint, BudgetCheckpoint::ModelCall);
        assert_eq!((s.used, s.requested, s.max), (801.0, 200.0, 1000.0));
        assert!(s.reason().contains("before a model call"), "{}", s.reason());
    }

    #[test]
    fn usd_is_inactive_for_an_unpriced_model_but_tokens_still_hold() {
        let b = RunBudget {
            limits: BudgetLimits {
                max_usd: Some(0.000_001),
                max_tokens: Some(150),
                ..BudgetLimits::default()
            },
            usd_enforced: false,
        };
        let unpriced = PricingTable::default();
        let mut p = probe(&b, &unpriced, usage(0, 0));
        p.next_call = Some(usage(50, 50));
        assert!(
            check(&p).is_none(),
            "usd must not fire for an unpriced model"
        );
        p.next_call = Some(usage(100, 100));
        assert_eq!(check(&p).unwrap().limit, BudgetLimitKind::Tokens);
    }

    #[test]
    fn usd_stops_when_priced() {
        let b = RunBudget {
            limits: BudgetLimits {
                max_usd: Some(0.001),
                ..BudgetLimits::default()
            },
            usd_enforced: true,
        };
        let t = priced();
        // used 900 tokens = $0.0009, next 200 = $0.0002 → 0.0011 > 0.001.
        let s = check(&probe(&b, &t, usage(450, 450))).unwrap();
        assert_eq!(s.limit, BudgetLimitKind::Usd);
    }

    #[test]
    fn wall_time_and_action_checkpoint() {
        let b = RunBudget {
            limits: BudgetLimits {
                max_wall_secs: Some(10),
                ..BudgetLimits::default()
            },
            usd_enforced: false,
        };
        let t = PricingTable::default();
        let mut p = probe(&b, &t, usage(0, 0));
        p.next_call = None;
        p.elapsed_secs = 9;
        assert!(check(&p).is_none());
        p.elapsed_secs = 10;
        let s = check(&p).unwrap();
        assert_eq!(
            (s.limit, s.checkpoint),
            (BudgetLimitKind::WallTime, BudgetCheckpoint::Action)
        );
    }

    #[test]
    fn daily_counts_the_principals_other_runs() {
        let t = priced();
        let p = BudgetProbe {
            budget: None,
            pricing: &t,
            model: "m",
            used: usage(0, 0),
            next_call: Some(usage(1000, 0)), // $0.001
            elapsed_secs: 0,
            daily: Some((0.01, 0.0095)),
            step: 0,
            branch: None,
        };
        assert_eq!(check(&p).unwrap().limit, BudgetLimitKind::DailyUsd);
        let p2 = BudgetProbe {
            daily: Some((0.01, 0.008)),
            ..p
        };
        assert!(check(&p2).is_none()); // 0.008 + 0.001 < 0.01
    }

    #[test]
    fn no_budget_never_stops() {
        let t = PricingTable::default();
        let p = BudgetProbe {
            budget: None,
            pricing: &t,
            model: "m",
            used: usage(u64::MAX / 4, u64::MAX / 4),
            next_call: Some(usage(u64::MAX / 4, 0)),
            elapsed_secs: u64::MAX,
            daily: None,
            step: 0,
            branch: None,
        };
        assert!(check(&p).is_none());
    }
}
