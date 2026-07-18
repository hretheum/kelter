//! The action space contract between the kernel and any policy oracle.
//!
//! The model *proposes* an action from a defined space; the kernel *validates
//! and executes*. `ValidatedAction` can only be constructed by
//! [`ActionSchema::validate`], so no adapter can hand the kernel an
//! unvalidated action — the type system enforces the trust boundary.

use serde::{Deserialize, Serialize};

/// Reserved argument name for tool-side idempotency (see
/// [`ActionSpec::supports_idempotency_key`]).
pub const IDEMPOTENCY_KEY_ARG: &str = "idempotency_key";

/// One action the policy may propose at a decision point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the action's arguments object.
    pub parameters: serde_json::Value,
}

impl ActionSpec {
    /// A tool that declares an `idempotency_key` property commits to the
    /// dedupe contract: calling it twice with the same key produces the
    /// effect exactly once. The kernel only re-executes a dangling side
    /// effect on resume when this holds (or fails the resume loudly).
    pub fn supports_idempotency_key(&self) -> bool {
        self.parameters
            .get("properties")
            .and_then(|p| p.get(IDEMPOTENCY_KEY_ARG))
            .is_some()
    }
}

/// The full action space offered to the policy oracle at one decision point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSchema {
    pub actions: Vec<ActionSpec>,
}

/// What a policy adapter returns: untrusted until the kernel validates it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedAction {
    pub action: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// Optional free-text rationale (never executed, only logged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// An action that passed kernel-side schema validation. Fields are private;
/// the only constructor is [`ActionSchema::validate`].
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedAction {
    action: String,
    arguments: serde_json::Value,
    rationale: Option<String>,
}

impl ValidatedAction {
    pub fn action(&self) -> &str {
        &self.action
    }
    pub fn arguments(&self) -> &serde_json::Value {
        &self.arguments
    }
    pub fn rationale(&self) -> Option<&str> {
        self.rationale.as_deref()
    }

    /// Kernel-side injection of the idempotency key into the arguments of a
    /// tool that declares support for it. Called after validation with a
    /// kernel-generated key — never with model-provided data.
    pub(crate) fn inject_idempotency_key(&mut self, key: &str) {
        if let Some(obj) = self.arguments.as_object_mut() {
            obj.insert(
                IDEMPOTENCY_KEY_ARG.to_string(),
                serde_json::Value::String(key.to_string()),
            );
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("proposed action '{0}' is not in the action space")]
    UnknownAction(String),
    #[error("arguments for '{action}' violate the schema: {details}")]
    InvalidArguments { action: String, details: String },
    #[error("action spec '{action}' has a malformed parameter schema: {details}")]
    BadSchema { action: String, details: String },
}

impl ActionSchema {
    pub fn spec(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|a| a.name == name)
    }

    /// Kernel-side validation of a proposed action against the action space.
    /// This runs for every adapter, regardless of how the adapter claims to
    /// have constrained the model — the kernel trusts no adapter.
    pub fn validate(&self, proposed: ProposedAction) -> Result<ValidatedAction, ValidationError> {
        let spec = self
            .spec(&proposed.action)
            .ok_or_else(|| ValidationError::UnknownAction(proposed.action.clone()))?;

        let validator = jsonschema::validator_for(&spec.parameters).map_err(|e| {
            ValidationError::BadSchema {
                action: spec.name.clone(),
                details: e.to_string(),
            }
        })?;

        let errors: Vec<String> = validator
            .iter_errors(&proposed.arguments)
            .map(|e| format!("{} (at {})", e, e.instance_path))
            .collect();
        if !errors.is_empty() {
            return Err(ValidationError::InvalidArguments {
                action: spec.name.clone(),
                details: errors.join("; "),
            });
        }

        Ok(ValidatedAction {
            action: proposed.action,
            arguments: proposed.arguments,
            rationale: proposed.rationale,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> ActionSchema {
        ActionSchema {
            actions: vec![ActionSpec {
                name: "write_file".into(),
                description: "Write a file".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            }],
        }
    }

    #[test]
    fn accepts_valid_action() {
        let v = schema()
            .validate(ProposedAction {
                action: "write_file".into(),
                arguments: json!({"path": "a.txt", "content": "hi"}),
                rationale: None,
            })
            .unwrap();
        assert_eq!(v.action(), "write_file");
    }

    #[test]
    fn rejects_unknown_action() {
        let err = schema()
            .validate(ProposedAction {
                action: "rm_rf".into(),
                arguments: json!({}),
                rationale: None,
            })
            .unwrap_err();
        assert!(matches!(err, ValidationError::UnknownAction(_)));
    }

    #[test]
    fn rejects_bad_arguments() {
        let err = schema()
            .validate(ProposedAction {
                action: "write_file".into(),
                arguments: json!({"path": 42}),
                rationale: None,
            })
            .unwrap_err();
        assert!(matches!(err, ValidationError::InvalidArguments { .. }));
    }

    #[test]
    fn rejects_extra_properties() {
        let err = schema()
            .validate(ProposedAction {
                action: "write_file".into(),
                arguments: json!({"path": "a", "content": "b", "sneaky": true}),
                rationale: None,
            })
            .unwrap_err();
        assert!(matches!(err, ValidationError::InvalidArguments { .. }));
    }
}
