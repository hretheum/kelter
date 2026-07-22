//! Rendering of the kernel's PolicyContext into model-facing text.
//! Shared by adapters; each adapter decides how to deliver it (raw prompt
//! with grammar vs. chat messages with native tools).

use kernel_core::{ActionSchema, PolicyContext};

/// System instruction describing the contract: pick ONE action from the
/// offered space and reply as a JSON envelope. Used by adapters that
/// constrain the raw completion (e.g. GBNF).
pub fn system_prompt(schema: &ActionSchema, context: &PolicyContext) -> String {
    let mut out = String::from(
        "You are the decision policy of an agent runtime. At each step you pick exactly one \
         action from the action space below. Respond ONLY with a JSON object of the form \
         {\"action\": \"<name>\", \"arguments\": { ... }} — no prose, no markdown.\n\n\
         Action space:\n",
    );
    push_actions_and_guidance(&mut out, schema, context);
    out
}

/// System instruction for adapters using native tool-calling: the action
/// space arrives as tools, so the model must always answer with exactly one
/// tool call (finishing via the `finish` tool), never with prose.
pub fn system_prompt_native(schema: &ActionSchema, context: &PolicyContext) -> String {
    let mut out = String::from(
        "You are the decision policy of an agent runtime. At each step respond by calling \
         EXACTLY ONE of the provided tools — never answer in prose. When the task's output \
         has been produced, call the `finish` tool with a short summary.\n\n\
         Available actions:\n",
    );
    push_actions_and_guidance(&mut out, schema, context);
    out
}

fn push_actions_and_guidance(out: &mut String, schema: &ActionSchema, context: &PolicyContext) {
    for action in &schema.actions {
        out.push_str(&format!(
            "- {}: {}\n  arguments schema: {}\n",
            action.name,
            action.description,
            serde_json::to_string(&action.parameters).unwrap_or_default()
        ));
    }
    if let Some(guidance) = &context.guidance {
        out.push_str(&format!("\nGuidance: {guidance}\n"));
    }
}

/// User-side message: the task plus everything that has happened so far.
/// Step results truncated at the historical default (600 chars).
pub fn user_prompt(context: &PolicyContext) -> String {
    user_prompt_with_limit(context, 600)
}

/// [`user_prompt`] with a configurable per-step-result truncation limit
/// (B3): 600 chars is too small for a compiler error, so coding setups
/// raise it via `[policy] step_result_limit`.
pub fn user_prompt_with_limit(context: &PolicyContext, step_result_limit: usize) -> String {
    let mut out = format!("Task: {}\n", context.task);

    if !context.memory.is_empty() {
        out.push_str("\nRelevant past episodes:\n");
        for m in &context.memory {
            out.push_str(&format!("- {m}\n"));
        }
    }

    if context.steps.is_empty() {
        out.push_str("\nNo steps executed yet. Choose the first action.\n");
    } else {
        out.push_str("\nSteps so far:\n");
        for s in &context.steps {
            out.push_str(&format!(
                "{}. {}({}) -> {} {}\n",
                s.index + 1,
                s.action,
                serde_json::to_string(&s.arguments).unwrap_or_default(),
                if s.ok { "ok" } else { "FAILED" },
                truncate(
                    &serde_json::to_string(&s.result).unwrap_or_default(),
                    step_result_limit
                ),
            ));
        }
        let done: Vec<&str> = context
            .steps
            .iter()
            .filter(|s| s.ok)
            .map(|s| s.action.as_str())
            .collect();
        if !done.is_empty() {
            out.push_str(&format!(
                "\nAlready completed successfully (do NOT repeat): {}.\n",
                done.join(", ")
            ));
        }
        out.push_str("Choose the next action (or finish if the output is produced).\n");
    }

    if let Some(err) = &context.last_error {
        out.push_str(&format!("\nIMPORTANT — previous attempt failed: {err}\n"));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…[truncated]", &s[..cut])
    }
}

/// Parse a model's JSON envelope into a ProposedAction. Tolerates
/// surrounding whitespace/code fences that laxer engines may emit.
pub fn parse_envelope(raw: &str) -> Result<kernel_core::ProposedAction, String> {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(trimmed)
        .map_err(|e| format!("unparseable action envelope: {e}: {trimmed}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_core::{ActionSpec, Latitude, StepRecord};
    use serde_json::json;

    fn ctx() -> PolicyContext {
        PolicyContext {
            task: "extract links".into(),
            steps: vec![StepRecord {
                index: 0,
                action: "read_source".into(),
                arguments: json!({"input": "a.md"}),
                result: json!({"ok": true}),
                ok: true,
            }],
            memory: vec!["task: similar | outcome: Success | steps: 3".into()],
            last_error: Some("bad args".into()),
            latitude: Latitude::Constrained,
            guidance: Some("one tool per step".into()),
            active_skills: Vec::new(),
        }
    }

    #[test]
    fn prompts_mention_all_parts() {
        let schema = ActionSchema {
            actions: vec![ActionSpec {
                name: "read_source".into(),
                description: "Read a source".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        };
        let sys = system_prompt(&schema, &ctx());
        assert!(sys.contains("read_source"));
        assert!(sys.contains("one tool per step"));
        let user = user_prompt(&ctx());
        assert!(user.contains("extract links"));
        assert!(user.contains("similar"));
        assert!(user.contains("bad args"));
    }

    #[test]
    fn parses_plain_and_fenced_envelopes() {
        let p = parse_envelope(r#"{"action": "finish", "arguments": {"summary": "x"}}"#).unwrap();
        assert_eq!(p.action, "finish");
        let p = parse_envelope("```json\n{\"action\": \"a\", \"arguments\": {}}\n```").unwrap();
        assert_eq!(p.action, "a");
        assert!(parse_envelope("not json").is_err());
    }
}
