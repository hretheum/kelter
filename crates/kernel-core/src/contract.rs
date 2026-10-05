//! The success contract (ADR 0037): an optional, declarative statement of
//! what must have *happened* for a run to count as successful, supplied by
//! the caller when the run is created and checked by the kernel — never by a
//! model — before it grants [`RunStatus::Completed`](crate::RunStatus).
//!
//! ADR 0033 set the floor: no effective step, no success. That floor cannot
//! tell "sent the e-mail" from "wrote a file and said it sent the e-mail".
//! The contract closes that gap for callers who know what the effect is: it
//! names the action that must have succeeded, and optionally a condition on
//! that action's recorded result. Every condition is a pure function of the
//! run's own step trace — the data the kernel already recorded — so the
//! verdict is deterministic, replayable and cheap. No code from the contract
//! is executed and no model is consulted.
//!
//! A contract is a conjunction: every condition must hold. Each condition is
//! existential over the run's steps: *some* successful step of the named
//! action satisfies it.

use serde::{Deserialize, Serialize};

use crate::engine::StepRecord;
use crate::outcome::is_kernel_owned;

/// Upper bound on conditions in one contract — a contract is a short
/// statement of intent, not a test suite.
pub const MAX_CONTRACT_CONDITIONS: usize = 16;
/// Upper bound on a condition's serialized expected value / substring.
pub const MAX_CONTRACT_VALUE_BYTES: usize = 4 * 1024;

/// What a run must have achieved to be `completed` (ADR 0037).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessContract {
    /// Conditions that must ALL hold. Never empty (validated at creation).
    pub require: Vec<ContractCondition>,
}

/// One deterministic condition over the run's step trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContractCondition {
    /// At least one step of `action` was dispatched and came back ok.
    ActionSucceeded { action: String },
    /// Some ok step of `action` has a result whose value at the JSON Pointer
    /// `pointer` (RFC 6901; `""` = the whole result) equals `equals`.
    ResultEquals {
        action: String,
        pointer: String,
        equals: serde_json::Value,
    },
    /// Some ok step of `action` has a STRING result at `pointer` containing
    /// `contains` (case-sensitive substring).
    ResultContains {
        action: String,
        pointer: String,
        contains: String,
    },
}

impl ContractCondition {
    /// The action this condition is about.
    pub fn action(&self) -> &str {
        match self {
            ContractCondition::ActionSucceeded { action }
            | ContractCondition::ResultEquals { action, .. }
            | ContractCondition::ResultContains { action, .. } => action,
        }
    }

    fn pointer(&self) -> Option<&str> {
        match self {
            ContractCondition::ActionSucceeded { .. } => None,
            ContractCondition::ResultEquals { pointer, .. }
            | ContractCondition::ResultContains { pointer, .. } => Some(pointer),
        }
    }

    /// Short human-readable rendering, used in reasons.
    pub fn describe(&self) -> String {
        match self {
            ContractCondition::ActionSucceeded { action } => {
                format!("action `{action}` succeeded")
            }
            ContractCondition::ResultEquals {
                action,
                pointer,
                equals,
            } => format!("`{action}` result at \"{pointer}\" equals {equals}"),
            ContractCondition::ResultContains {
                action,
                pointer,
                contains,
            } => format!("`{action}` result at \"{pointer}\" contains {contains:?}"),
        }
    }
}

/// How a successful verdict was reached (ADR 0037). Set by the kernel on
/// every run it decides success for (`completed` and `incomplete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// No contract: the kernel applied only the ADR 0033 floor (some
    /// effective step). A `completed` run with this basis is UNVERIFIED —
    /// excluded from learning until a human confirms it.
    None,
    /// The run carried a contract and the kernel evaluated it.
    Contract,
}

/// The kernel's evaluation of a contract, one entry per condition, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractReport {
    pub satisfied: bool,
    pub conditions: Vec<ConditionResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConditionResult {
    pub condition: ContractCondition,
    pub met: bool,
    /// Why it was (not) met, in terms of the trace.
    pub detail: String,
}

impl ContractReport {
    /// The `error` an `incomplete` run carries when its contract failed:
    /// names every condition that did not hold.
    pub fn unmet_reason(&self) -> String {
        let unmet: Vec<String> = self
            .conditions
            .iter()
            .filter(|c| !c.met)
            .map(|c| format!("{} — {}", c.condition.describe(), c.detail))
            .collect();
        format!(
            "success contract not met ({} of {} condition(s) failed): {} (the model's summary \
             is kept as output, unverified)",
            unmet.len(),
            self.conditions.len(),
            unmet.join("; ")
        )
    }
}

fn valid_pointer(p: &str) -> bool {
    p.is_empty() || p.starts_with('/')
}

/// Validate a contract's SHAPE and its actions against the action names the
/// kernel currently offers (`known_actions`). Called at run creation, so a
/// bad contract is a 4xx up front rather than a silent `incomplete` later.
pub fn validate(contract: &SuccessContract, known_actions: &[String]) -> Result<(), String> {
    if contract.require.is_empty() {
        return Err("success_contract.require must name at least one condition".into());
    }
    if contract.require.len() > MAX_CONTRACT_CONDITIONS {
        return Err(format!(
            "success_contract.require has {} conditions (max {MAX_CONTRACT_CONDITIONS})",
            contract.require.len()
        ));
    }
    for (i, c) in contract.require.iter().enumerate() {
        let action = c.action();
        if action.trim().is_empty() {
            return Err(format!(
                "success_contract.require[{i}]: action must not be empty"
            ));
        }
        if is_kernel_owned(action) {
            return Err(format!(
                "success_contract.require[{i}]: `{action}` is kernel-owned — it never \
                 dispatches anything, so it cannot evidence success"
            ));
        }
        if !known_actions.iter().any(|a| a == action) {
            return Err(format!(
                "success_contract.require[{i}]: unknown action `{action}` (not in the action \
                 space this daemon offers; see GET /api/v1/tools)"
            ));
        }
        if let Some(p) = c.pointer() {
            if !valid_pointer(p) {
                return Err(format!(
                    "success_contract.require[{i}]: pointer \"{p}\" is not a JSON Pointer (must \
                     be empty or start with '/')"
                ));
            }
        }
        let value_bytes = match c {
            ContractCondition::ActionSucceeded { .. } => 0,
            ContractCondition::ResultEquals { equals, .. } => equals.to_string().len(),
            ContractCondition::ResultContains { contains, .. } => {
                if contains.is_empty() {
                    return Err(format!(
                        "success_contract.require[{i}]: contains must not be empty"
                    ));
                }
                contains.len()
            }
        };
        if value_bytes > MAX_CONTRACT_VALUE_BYTES {
            return Err(format!(
                "success_contract.require[{i}]: expected value is {value_bytes} bytes (max \
                 {MAX_CONTRACT_VALUE_BYTES})"
            ));
        }
    }
    Ok(())
}

fn evaluate_condition(c: &ContractCondition, steps: &[StepRecord]) -> ConditionResult {
    let action = c.action();
    let attempts: Vec<&StepRecord> = steps.iter().filter(|s| s.action == action).collect();
    let ok: Vec<&StepRecord> = attempts.iter().copied().filter(|s| s.ok).collect();
    let (met, detail) = if attempts.is_empty() {
        (false, format!("`{action}` was never executed"))
    } else if ok.is_empty() {
        (
            false,
            format!("`{action}` ran {} time(s), all failed", attempts.len()),
        )
    } else {
        match c {
            ContractCondition::ActionSucceeded { .. } => {
                (true, format!("`{action}` succeeded {} time(s)", ok.len()))
            }
            ContractCondition::ResultEquals {
                pointer, equals, ..
            } => {
                let hit = ok
                    .iter()
                    .any(|s| s.result.pointer(pointer).is_some_and(|v| v == equals));
                if hit {
                    (true, format!("a successful `{action}` result matches"))
                } else {
                    (
                        false,
                        format!(
                            "no successful `{action}` result ({} checked) has {equals} at \
                             \"{pointer}\"",
                            ok.len()
                        ),
                    )
                }
            }
            ContractCondition::ResultContains {
                pointer, contains, ..
            } => {
                let hit = ok.iter().any(|s| {
                    s.result
                        .pointer(pointer)
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| v.contains(contains.as_str()))
                });
                if hit {
                    (true, format!("a successful `{action}` result matches"))
                } else {
                    (
                        false,
                        format!(
                            "no successful `{action}` result ({} checked) has a string \
                             containing {contains:?} at \"{pointer}\"",
                            ok.len()
                        ),
                    )
                }
            }
        }
    };
    ConditionResult {
        condition: c.clone(),
        met,
        detail,
    }
}

/// Evaluate `contract` against a run's step trace. Pure and deterministic.
pub fn evaluate(contract: &SuccessContract, steps: &[StepRecord]) -> ContractReport {
    let conditions: Vec<ConditionResult> = contract
        .require
        .iter()
        .map(|c| evaluate_condition(c, steps))
        .collect();
    ContractReport {
        satisfied: conditions.iter().all(|c| c.met),
        conditions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(action: &str, ok: bool, result: serde_json::Value) -> StepRecord {
        StepRecord {
            index: 0,
            action: action.into(),
            arguments: json!({}),
            result,
            ok,
        }
    }

    fn known() -> Vec<String> {
        vec!["send_email".into(), "write_file".into()]
    }

    fn c(v: serde_json::Value) -> SuccessContract {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn json_shape_round_trips() {
        let k = c(json!({"require": [
            {"kind": "action_succeeded", "action": "send_email"},
            {"kind": "result_equals", "action": "send_email", "pointer": "/status", "equals": "sent"},
            {"kind": "result_contains", "action": "send_email", "pointer": "", "contains": "ok"}
        ]}));
        assert_eq!(k.require.len(), 3);
        let back: SuccessContract =
            serde_json::from_value(serde_json::to_value(&k).unwrap()).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn unknown_fields_and_kinds_are_refused_by_the_parser() {
        assert!(serde_json::from_value::<SuccessContract>(
            json!({"require": [{"kind": "action_succeeded", "action": "x", "extra": 1}]})
        )
        .is_err());
        assert!(serde_json::from_value::<SuccessContract>(
            json!({"require": [{"kind": "model_says_so", "action": "x"}]})
        )
        .is_err());
        assert!(serde_json::from_value::<SuccessContract>(json!({"requires": []})).is_err());
    }

    #[test]
    fn validation_refuses_empty_unknown_kernel_owned_and_bad_pointer() {
        assert!(validate(&c(json!({"require": []})), &known()).is_err());
        let e = validate(
            &c(json!({"require": [{"kind": "action_succeeded", "action": "fax"}]})),
            &known(),
        )
        .unwrap_err();
        assert!(e.contains("unknown action `fax`"), "{e}");
        let e = validate(
            &c(json!({"require": [{"kind": "action_succeeded", "action": "finish"}]})),
            &["finish".to_string()],
        )
        .unwrap_err();
        assert!(e.contains("kernel-owned"), "{e}");
        let e = validate(
            &c(
                json!({"require": [{"kind": "result_equals", "action": "send_email",
                                   "pointer": "status", "equals": 1}]}),
            ),
            &known(),
        )
        .unwrap_err();
        assert!(e.contains("JSON Pointer"), "{e}");
        assert!(validate(
            &c(json!({"require": [{"kind": "action_succeeded", "action": "send_email"}]})),
            &known()
        )
        .is_ok());
    }

    #[test]
    fn write_file_does_not_satisfy_send_email() {
        let k = c(json!({"require": [{"kind": "action_succeeded", "action": "send_email"}]}));
        let r = evaluate(&k, &[step("write_file", true, json!({}))]);
        assert!(!r.satisfied);
        assert!(r.conditions[0].detail.contains("never executed"));
        assert!(r.unmet_reason().contains("action `send_email` succeeded"));
    }

    #[test]
    fn failed_attempts_do_not_satisfy() {
        let k = c(json!({"require": [{"kind": "action_succeeded", "action": "send_email"}]}));
        let r = evaluate(&k, &[step("send_email", false, json!({}))]);
        assert!(!r.satisfied);
        assert!(r.conditions[0].detail.contains("all failed"));
    }

    #[test]
    fn result_conditions_check_some_ok_step() {
        let k = c(json!({"require": [
            {"kind": "result_equals", "action": "send_email", "pointer": "/status", "equals": "sent"},
            {"kind": "result_contains", "action": "send_email", "pointer": "/msg", "contains": "queued"}
        ]}));
        let steps = [
            step(
                "send_email",
                false,
                json!({"status": "sent", "msg": "queued"}),
            ),
            step(
                "send_email",
                true,
                json!({"status": "deferred", "msg": "x"}),
            ),
        ];
        let r = evaluate(&k, &steps);
        assert!(!r.satisfied);
        assert!(!r.conditions[0].met && !r.conditions[1].met);

        let steps = [
            step("send_email", true, json!({"status": "deferred"})),
            step(
                "send_email",
                true,
                json!({"status": "sent", "msg": "id 7 queued"}),
            ),
        ];
        assert!(evaluate(&k, &steps).satisfied);
    }

    #[test]
    fn result_contains_requires_a_string() {
        let k = c(json!({"require": [
            {"kind": "result_contains", "action": "send_email", "pointer": "/n", "contains": "1"}
        ]}));
        assert!(!evaluate(&k, &[step("send_email", true, json!({"n": 1}))]).satisfied);
    }
}
