//! ActionSchema → GBNF grammar for llama.cpp constrained decoding.
//!
//! The generated grammar forces the decoder to emit exactly
//! `{"action": "<one of the offered names>", "arguments": { ...typed... }}`.
//! A weak model *cannot* produce a malformed tool call: correctness of the
//! envelope is guaranteed by the decoder, not by hoping the model complies.
//! The kernel still re-validates on top (it trusts no adapter).
//!
//! Supported argument schemas: objects whose properties are strings, string
//! enums, integers, numbers, booleans, nested objects and arrays; anything
//! else falls back to a generic JSON value rule. Every declared property is
//! emitted in declaration order — JSON Schema treats extra optional
//! properties as valid, so this stays within the schema.

use kernel_core::ActionSchema;

/// Escape a Rust string into a GBNF double-quoted literal.
fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON string literal as a GBNF literal (includes the JSON quotes).
fn json_key(s: &str) -> String {
    lit(&format!("\"{s}\""))
}

struct Generator {
    rules: Vec<(String, String)>,
    counter: usize,
}

impl Generator {
    fn fresh(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-{}", self.counter)
    }

    /// Returns the rule *expression* for a value matching `schema`.
    fn value_rule(&mut self, schema: &serde_json::Value) -> String {
        // String enums become literal alternatives — the tightest constraint.
        if let Some(enum_vals) = schema.get("enum").and_then(|e| e.as_array()) {
            let alts: Vec<String> = enum_vals
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| lit(&format!("\"{s}\"")))
                .collect();
            if !alts.is_empty() {
                return alts.join(" | ");
            }
        }
        match schema.get("type").and_then(|t| t.as_str()) {
            Some("string") => "string".into(),
            Some("integer") => "integer".into(),
            Some("number") => "number".into(),
            Some("boolean") => "boolean".into(),
            Some("object") => {
                if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                    let name = self.fresh("obj");
                    let body = self.object_body(props);
                    self.rules.push((name.clone(), body));
                    name
                } else if schema
                    .get("additionalProperties")
                    .and_then(|ap| ap.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("string")
                {
                    // A bounded string→string map: cheap for the decoder's
                    // grammar sampler (no recursion into the generic `value`
                    // rule, which is pathologically slow on weak models).
                    let name = self.fresh("smap");
                    self.rules.push((
                        name.clone(),
                        "\"{\" ws ( string ws \":\" ws string ( ws \",\" ws string ws \":\" ws \
                         string )* )? ws \"}\""
                            .into(),
                    ));
                    name
                } else {
                    "object".into()
                }
            }
            Some("array") => {
                if let Some(items) = schema.get("items") {
                    // Group the item expression for the same precedence
                    // reason as object properties (an enum item would
                    // otherwise let `|` escape the repetition).
                    let inner = format!("( {} )", self.value_rule(items));
                    let name = self.fresh("arr");
                    self.rules.push((
                        name.clone(),
                        format!("\"[\" ws ( {inner} ( ws \",\" ws {inner} )* )? ws \"]\""),
                    ));
                    name
                } else {
                    "array".into()
                }
            }
            _ => "value".into(),
        }
    }

    /// Object body forcing every declared property, in declaration order.
    fn object_body(&mut self, props: &serde_json::Map<String, serde_json::Value>) -> String {
        if props.is_empty() {
            return "\"{\" ws \"}\"".into();
        }
        let mut parts = vec!["\"{\" ws".to_string()];
        for (i, (key, prop_schema)) in props.iter().enumerate() {
            if i > 0 {
                parts.push("\",\" ws".into());
            }
            let vrule = self.value_rule(prop_schema);
            // Parenthesize the value expression: an enum/alternation value
            // (`"a" | "b"`) inlined bare into this sequence would let `|`
            // (lowest precedence in GBNF) split the whole object rule.
            parts.push(format!("{} ws \":\" ws ( {} ) ws", json_key(key), vrule));
        }
        parts.push("\"}\"".into());
        parts.join(" ")
    }
}

/// Generate a complete GBNF grammar for one decision point.
pub fn grammar_for(schema: &ActionSchema) -> String {
    let mut generator = Generator {
        rules: Vec::new(),
        counter: 0,
    };

    let mut alts = Vec::new();
    for (i, action) in schema.actions.iter().enumerate() {
        let args_expr = match action
            .parameters
            .get("properties")
            .and_then(|p| p.as_object())
        {
            Some(props) => {
                let name = format!("args-{i}");
                let body = generator.object_body(props);
                generator.rules.push((name.clone(), body));
                name
            }
            None => "object".to_string(),
        };
        let alt_name = format!("act-{i}");
        generator.rules.push((
            alt_name.clone(),
            format!(
                "\"{{\" ws {action_key} ws \":\" ws {name} ws \",\" ws {args_key} ws \":\" ws {args_expr} ws \"}}\"",
                action_key = json_key("action"),
                name = lit(&format!("\"{}\"", action.name)),
                args_key = json_key("arguments"),
            ),
        ));
        alts.push(alt_name);
    }

    let mut out = String::new();
    out.push_str(&format!("root ::= {}\n", alts.join(" | ")));
    for (name, body) in &generator.rules {
        out.push_str(&format!("{name} ::= {body}\n"));
    }
    // Generic JSON primitives (subset of the canonical json.gbnf shipped
    // with llama.cpp).
    out.push_str(concat!(
        "value ::= object | array | string | number | boolean | null\n",
        "object ::= \"{\" ws ( string ws \":\" ws value ( ws \",\" ws string ws \":\" ws value )* )? ws \"}\"\n",
        "array ::= \"[\" ws ( value ( ws \",\" ws value )* )? ws \"]\"\n",
        "string ::= \"\\\"\" ( [^\"\\\\\\x7F\\x00-\\x1F] | \"\\\\\" ([\"\\\\bfnrt] | \"u\" [0-9a-fA-F]{4}) )* \"\\\"\"\n",
        "number ::= \"-\"? ([0-9] | [1-9] [0-9]*) (\".\" [0-9]+)? ([eE] [-+]? [0-9]+)?\n",
        "integer ::= \"-\"? ([0-9] | [1-9] [0-9]*)\n",
        "boolean ::= \"true\" | \"false\"\n",
        "null ::= \"null\"\n",
        "ws ::= [ \\t\\n\\r]*\n",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_core::ActionSpec;
    use serde_json::json;

    fn schema() -> ActionSchema {
        ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "read_source".into(),
                    description: "read".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"},
                            "save_as": {"type": "string"}
                        },
                        "required": ["input", "save_as"],
                        "additionalProperties": false
                    }),
                },
                ActionSpec {
                    name: "write_report".into(),
                    description: "write".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "format": {"type": "string", "enum": ["json", "markdown"]},
                            "limit": {"type": "integer"}
                        },
                        "required": ["format"],
                        "additionalProperties": false
                    }),
                },
            ],
        }
    }

    #[test]
    fn grammar_enumerates_action_names_as_literals() {
        let g = grammar_for(&schema());
        assert!(g.contains(r#""\"read_source\"""#));
        assert!(g.contains(r#""\"write_report\"""#));
        assert!(g.starts_with("root ::= act-0 | act-1\n"));
    }

    #[test]
    fn grammar_types_arguments() {
        let g = grammar_for(&schema());
        // enum → literal alternatives, integer → integer rule
        assert!(g.contains(r#""\"json\"" | "\"markdown\"""#));
        assert!(g.contains("integer"));
        // every property key appears as a quoted JSON key literal
        assert!(g.contains(r#""\"input\"""#));
        assert!(g.contains(r#""\"save_as\"""#));
    }

    #[test]
    fn grammar_contains_json_primitives_and_no_dangling_rules() {
        let g = grammar_for(&schema());
        for rule in ["string ::=", "ws ::=", "boolean ::=", "number ::="] {
            assert!(g.contains(rule), "missing {rule}");
        }
        // Every referenced act-N/args-N rule is defined.
        for i in 0..2 {
            assert!(g.contains(&format!("act-{i} ::=")));
            assert!(g.contains(&format!("args-{i} ::=")));
        }
    }

    #[test]
    fn empty_properties_yield_empty_object() {
        let g = grammar_for(&ActionSchema {
            actions: vec![ActionSpec {
                name: "noop".into(),
                description: "".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        });
        assert!(g.contains(r#"args-0 ::= "{" ws "}""#));
    }
}
