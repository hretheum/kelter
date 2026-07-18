//! Plan mode: the model proposes a DAG of steps; the kernel validates it
//! before executing anything, then runs it durably (ADR 0010).
//!
//! A plan is **one proposed action** — it does not add a new execution
//! surface. Each node executes through an existing seam:
//! - `tool` → an action in the MCP action space,
//! - `a2a`  → a `delegate_to_<name>` action (the A2A source, still in the
//!   action space) — inheriting Phase 4b egress governance,
//! - `review` → a kernel-owned decision point (approve/reject).
//!
//! Validation (this module) is an *additional* layer: it rejects a
//! malformed DAG (cycles, unknown node types, references to actions the
//! registry does not offer) BEFORE any node runs. Per-node validation and
//! journaling still happen during execution — the plan never bypasses them.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::action::ActionSchema;

/// The kernel-owned action a plan node of type `review` resolves to.
pub const REVIEW_ACTION: &str = "review_verdict";
/// The action the model proposes to enter plan mode.
pub const PROPOSE_PLAN_ACTION: &str = "propose_plan";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// Executes a tool from the MCP action space.
    Tool,
    /// Delegates to a remote agent (`delegate_to_<name>`); A2A governance.
    A2a,
    /// A kernel-owned review/approval decision over upstream node outputs.
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNode {
    /// Unique within the plan.
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    /// For `tool`/`a2a`: the action name to run. Ignored for `review`.
    #[serde(default)]
    pub action: Option<String>,
    /// For `tool`/`a2a`: the action arguments. For `review`: optional.
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// Node ids this node depends on (its inputs). Empty = a root.
    #[serde(default)]
    pub deps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub nodes: Vec<PlanNode>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("plan is empty")]
    Empty,
    #[error("duplicate node id '{0}'")]
    DuplicateId(String),
    #[error("node '{node}' depends on unknown node '{dep}'")]
    UnknownDep { node: String, dep: String },
    #[error("plan contains a cycle among nodes: {0:?}")]
    Cycle(Vec<String>),
    #[error("node '{0}' ({1:?}) must declare an `action`")]
    MissingAction(String, NodeType),
    #[error(
        "node '{node}' references action '{action}', which is not in the registry's action space"
    )]
    UnknownAction { node: String, action: String },
    #[error("node '{node}' is typed '{declared}' but action '{action}' is a {actual} action")]
    NodeTypeMismatch {
        node: String,
        declared: &'static str,
        action: String,
        actual: &'static str,
    },
    #[error("plan has {0} nodes, exceeding the limit of {1}")]
    TooLarge(usize, usize),
}

/// A validated plan: nodes in a deterministic execution order (topological,
/// ties broken by declaration order). Constructible only via [`validate`].
#[derive(Debug, Clone)]
pub struct ValidatedPlan {
    plan: Plan,
    /// Indices into `plan.nodes`, in execution order.
    order: Vec<usize>,
}

impl ValidatedPlan {
    pub fn nodes_in_order(&self) -> impl Iterator<Item = (usize, &PlanNode)> {
        self.order.iter().map(move |&i| (i, &self.plan.nodes[i]))
    }
    pub fn node(&self, index: usize) -> &PlanNode {
        &self.plan.nodes[index]
    }
    pub fn len(&self) -> usize {
        self.plan.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.plan.nodes.is_empty()
    }
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
    /// The id → execution result map is built by the engine; this exposes
    /// the dependency ids so the engine can assemble a node's input context.
    pub fn deps_of(&self, index: usize) -> &[String] {
        &self.plan.nodes[index].deps
    }
}

const MAX_NODES: usize = 32;

/// Validate a proposed plan against the live action space. Returns the plan
/// with a deterministic execution order, or the first structural error.
///
/// `is_a2a_action` classifies an action name as an A2A delegation (vs a
/// plain tool) so node-type declarations can be checked against reality.
pub fn validate(
    plan: Plan,
    schema: &ActionSchema,
    is_a2a_action: impl Fn(&str) -> bool,
) -> Result<ValidatedPlan, PlanError> {
    if plan.nodes.is_empty() {
        return Err(PlanError::Empty);
    }
    if plan.nodes.len() > MAX_NODES {
        return Err(PlanError::TooLarge(plan.nodes.len(), MAX_NODES));
    }

    // Unique ids.
    let mut ids: HashSet<&str> = HashSet::new();
    for node in &plan.nodes {
        if !ids.insert(node.id.as_str()) {
            return Err(PlanError::DuplicateId(node.id.clone()));
        }
    }

    // Per-node type/action checks + dep existence.
    for node in &plan.nodes {
        for dep in &node.deps {
            if !ids.contains(dep.as_str()) {
                return Err(PlanError::UnknownDep {
                    node: node.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
        match node.node_type {
            NodeType::Review => {}
            NodeType::Tool | NodeType::A2a => {
                let action = node
                    .action
                    .as_deref()
                    .ok_or_else(|| PlanError::MissingAction(node.id.clone(), node.node_type))?;
                if !schema.actions.iter().any(|a| a.name == action) {
                    return Err(PlanError::UnknownAction {
                        node: node.id.clone(),
                        action: action.to_string(),
                    });
                }
                // The declared node type must match how the registry
                // classifies the action (a `tool` node can't point at a
                // remote agent and vice versa).
                let actual_is_a2a = is_a2a_action(action);
                let declared_is_a2a = node.node_type == NodeType::A2a;
                if actual_is_a2a != declared_is_a2a {
                    return Err(PlanError::NodeTypeMismatch {
                        node: node.id.clone(),
                        declared: if declared_is_a2a { "a2a" } else { "tool" },
                        action: action.to_string(),
                        actual: if actual_is_a2a { "a2a" } else { "tool" },
                    });
                }
            }
        }
    }

    // Topological sort (Kahn) — detects cycles and yields execution order.
    let index_of: HashMap<&str, usize> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut indegree = vec![0usize; plan.nodes.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); plan.nodes.len()];
    for (i, node) in plan.nodes.iter().enumerate() {
        indegree[i] = node.deps.len();
        for dep in &node.deps {
            dependents[index_of[dep.as_str()]].push(i);
        }
    }

    // Ready set processed in declaration order for determinism.
    let mut order = Vec::with_capacity(plan.nodes.len());
    let mut ready: Vec<usize> = (0..plan.nodes.len())
        .filter(|&i| indegree[i] == 0)
        .collect();
    ready.sort_unstable();
    while let Some(i) = pop_min(&mut ready) {
        order.push(i);
        for &d in &dependents[i] {
            indegree[d] -= 1;
            if indegree[d] == 0 {
                ready.push(d);
            }
        }
    }

    if order.len() != plan.nodes.len() {
        let unresolved: Vec<String> = (0..plan.nodes.len())
            .filter(|i| !order.contains(i))
            .map(|i| plan.nodes[i].id.clone())
            .collect();
        return Err(PlanError::Cycle(unresolved));
    }

    Ok(ValidatedPlan { plan, order })
}

fn pop_min(ready: &mut Vec<usize>) -> Option<usize> {
    if ready.is_empty() {
        return None;
    }
    let (pos, _) = ready.iter().enumerate().min_by_key(|(_, v)| **v)?;
    Some(ready.swap_remove(pos))
}

/// JSON Schema for the plan-proposal action's arguments (GBNF-constrainable
/// subset, ADR 0003). Kept deliberately small so a weak model can emit a
/// valid — even single-node — plan.
pub fn plan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "nodes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "type": {"type": "string", "enum": ["tool", "a2a", "review"]},
                        "action": {"type": "string"},
                        // A shallow string→string map under constrained
                        // decoding (keeps the grammar cheap on weak models).
                        // The kernel re-validates each node's arguments
                        // against the real tool schema at execution time.
                        "arguments": {"type": "object", "additionalProperties": {"type": "string"}},
                        "deps": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["id", "type"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["nodes"],
        "additionalProperties": false
    })
}

/// The kernel-owned action space offered at a `review` node.
pub fn review_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "approve": {"type": "boolean"},
            "reason": {"type": "string"}
        },
        "required": ["approve", "reason"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionSpec;
    use serde_json::json;

    fn schema() -> ActionSchema {
        ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "read_source".into(),
                    description: "".into(),
                    parameters: json!({"type": "object"}),
                },
                ActionSpec {
                    name: "delegate_to_toy".into(),
                    description: "".into(),
                    parameters: json!({"type": "object"}),
                },
            ],
        }
    }

    fn is_a2a(name: &str) -> bool {
        name.starts_with("delegate_to_")
    }

    fn node(id: &str, ty: NodeType, action: Option<&str>, deps: &[&str]) -> PlanNode {
        PlanNode {
            id: id.into(),
            node_type: ty,
            action: action.map(String::from),
            arguments: json!({}),
            deps: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn valid_linear_plan_orders_topologically() {
        let plan = Plan {
            nodes: vec![
                node("b", NodeType::Review, None, &["a"]),
                node("a", NodeType::Tool, Some("read_source"), &[]),
            ],
        };
        let v = validate(plan, &schema(), is_a2a).unwrap();
        let order: Vec<&str> = v.nodes_in_order().map(|(_, n)| n.id.as_str()).collect();
        assert_eq!(order, vec!["a", "b"]); // dep before dependent
    }

    #[test]
    fn valid_diamond_dag() {
        let plan = Plan {
            nodes: vec![
                node("root", NodeType::Tool, Some("read_source"), &[]),
                node("left", NodeType::Tool, Some("read_source"), &["root"]),
                node("right", NodeType::A2a, Some("delegate_to_toy"), &["root"]),
                node("join", NodeType::Review, None, &["left", "right"]),
            ],
        };
        let v = validate(plan, &schema(), is_a2a).unwrap();
        assert_eq!(v.len(), 4);
        let order: Vec<&str> = v.nodes_in_order().map(|(_, n)| n.id.as_str()).collect();
        assert_eq!(order[0], "root");
        assert_eq!(order[3], "join");
    }

    #[test]
    fn cycle_is_rejected() {
        let plan = Plan {
            nodes: vec![
                node("a", NodeType::Tool, Some("read_source"), &["b"]),
                node("b", NodeType::Tool, Some("read_source"), &["a"]),
            ],
        };
        assert!(matches!(
            validate(plan, &schema(), is_a2a),
            Err(PlanError::Cycle(_))
        ));
    }

    #[test]
    fn unknown_action_rejected() {
        let plan = Plan {
            nodes: vec![node("a", NodeType::Tool, Some("nonexistent"), &[])],
        };
        assert!(matches!(
            validate(plan, &schema(), is_a2a),
            Err(PlanError::UnknownAction { .. })
        ));
    }

    #[test]
    fn node_type_mismatch_rejected() {
        // 'tool' node pointing at an a2a action.
        let plan = Plan {
            nodes: vec![node("a", NodeType::Tool, Some("delegate_to_toy"), &[])],
        };
        assert!(matches!(
            validate(plan, &schema(), is_a2a),
            Err(PlanError::NodeTypeMismatch { .. })
        ));
    }

    #[test]
    fn unknown_dep_rejected() {
        let plan = Plan {
            nodes: vec![node("a", NodeType::Tool, Some("read_source"), &["ghost"])],
        };
        assert!(matches!(
            validate(plan, &schema(), is_a2a),
            Err(PlanError::UnknownDep { .. })
        ));
    }

    #[test]
    fn duplicate_id_rejected() {
        let plan = Plan {
            nodes: vec![
                node("a", NodeType::Tool, Some("read_source"), &[]),
                node("a", NodeType::Review, None, &[]),
            ],
        };
        assert!(matches!(
            validate(plan, &schema(), is_a2a),
            Err(PlanError::DuplicateId(_))
        ));
    }

    #[test]
    fn empty_plan_rejected() {
        assert!(matches!(
            validate(Plan { nodes: vec![] }, &schema(), is_a2a),
            Err(PlanError::Empty)
        ));
    }

    #[test]
    fn review_without_action_is_ok() {
        let plan = Plan {
            nodes: vec![node("r", NodeType::Review, None, &[])],
        };
        assert!(validate(plan, &schema(), is_a2a).is_ok());
    }
}
