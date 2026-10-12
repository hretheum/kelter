//! The `local` adapters: llama.cpp server over HTTP.
//!
//! Two variants against the same server:
//! - [`LocalAdapter`] (`local:<model>`) — GBNF constrained decoding: the
//!   action envelope is enforced by the *decoder* — a weak model cannot emit
//!   a malformed tool call. This is the "weak but reliable" path.
//! - [`LocalNativeAdapter`] (`local-native:<model>`) — native `tools`
//!   field (C1): for models whose chat template fights GBNF (measured:
//!   `gpt-oss-20b` answers HTTP 500 in ~90% of GBNF calls but 7/7 through
//!   native tools). The kernel still re-validates on top, as everywhere.
//!
//! Verified against llama.cpp b10050: `/v1/chat/completions` accepts the
//! llama.cpp `grammar` extension field and applies the model's chat template
//! server-side (see docs/BUILD-EVIDENCE.md). Thinking-mode control (B1) rides
//! `chat_template_kwargs.enable_thinking` — the llama.cpp/Qwen convention.

use async_trait::async_trait;
use serde::Deserialize;

use kernel_core::{
    ActionSchema, CallBound, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    Usage,
};

use crate::gbnf;
use crate::prompt;
use crate::AdapterTuning;

pub struct LocalAdapter {
    client: reqwest::Client,
    base_url: String,
    model: String,
    tuning: AdapterTuning,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageBody>,
}

#[derive(Deserialize)]
struct UsageBody {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
    /// Thinking-mode models (Qwen family) put their chain of thought here;
    /// under a grammar the whole constrained JSON can land here while
    /// `content` stays empty — an adapter-configuration problem, not a
    /// model failure (B1).
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Deserialize)]
struct ToolCall {
    function: FunctionCall,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    /// JSON-encoded string per the OpenAI-compatible contract.
    arguments: String,
}

/// Strip a trailing `/v1` so `.../8080` and `.../11434/v1` both resolve to
/// exactly one `/v1/chat/completions`.
fn normalize_base(raw: String) -> String {
    let b = raw.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// The empty-completion diagnosis (B1): reasoning-only responses get an
/// actionable error naming the fix, instead of a bare "empty completion".
fn empty_content_error(msg: &ChatMessage, finish_reason: Option<&str>) -> PolicyError {
    let reasoning_len = msg.reasoning_content.as_deref().map(str::len).unwrap_or(0);
    if reasoning_len > 0 {
        PolicyError::Malformed(format!(
            "model returned ONLY reasoning_content ({reasoning_len} chars, finish_reason={}) and \
             an empty content — this is a thinking-mode configuration problem, not a model \
             failure: set [policy] thinking = false (Qwen-family models require it under a \
             grammar)",
            finish_reason.unwrap_or("?")
        ))
    } else {
        PolicyError::Malformed(format!(
            "empty completion (finish_reason={})",
            finish_reason.unwrap_or("?")
        ))
    }
}

impl LocalAdapter {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_tuning(base_url, model, AdapterTuning::default())
    }

    pub fn with_tuning(
        base_url: impl Into<String>,
        model: impl Into<String>,
        tuning: AdapterTuning,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: normalize_base(base_url.into()),
            model: model.into(),
            tuning,
        }
    }
}

#[async_trait]
impl PolicyOracle for LocalAdapter {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let grammar = gbnf::grammar_for(schema);
        let limit = self.tuning.step_result_limit.unwrap_or(600);
        let mut body = serde_json::json!({
            // llama-server ignores the model name (it serves one model),
            // but the field keeps the request OpenAI-shaped.
            "model": self.model,
            "messages": [
                {"role": "system", "content": prompt::system_prompt(schema, context)},
                {"role": "user", "content": prompt::user_prompt_with_limit(context, limit)},
            ],
            // llama.cpp extension: GBNF grammar constraining the decoder.
            "grammar": grammar,
            "temperature": 0.0,
            "max_tokens": self.tuning.max_tokens.unwrap_or(512),
        });
        if let Some(thinking) = self.tuning.thinking {
            body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": thinking });
        }

        let res = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| PolicyError::Backend(format!("llama.cpp server unreachable: {e}")))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            return Err(PolicyError::Backend(format!(
                "llama.cpp server returned {status}: {text}"
            )));
        }

        let parsed: ChatResponse = res
            .json()
            .await
            .map_err(|e| PolicyError::Malformed(format!("bad response json: {e}")))?;
        let usage = parsed.usage.as_ref().map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            calls: 1,
        });
        let choice = parsed
            .choices
            .first()
            .ok_or_else(|| PolicyError::Malformed("no choices in response".into()))?;
        let content = match choice.message.content.as_deref() {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return Err(empty_content_error(
                    &choice.message,
                    choice.finish_reason.as_deref(),
                ))
            }
        };

        let action = prompt::parse_envelope(content).map_err(PolicyError::Malformed)?;
        Ok(Proposal { action, usage })
    }

    fn call_bound(&self, context: &PolicyContext, schema: &ActionSchema) -> CallBound {
        let limit = self.tuning.step_result_limit.unwrap_or(600);
        // The GBNF grammar constrains decoding; it is not prompt text.
        crate::chat_call_bound(
            &[
                &prompt::system_prompt(schema, context),
                &prompt::user_prompt_with_limit(context, limit),
            ],
            None,
            self.tuning.max_tokens.unwrap_or(512),
        )
    }

    fn describe(&self) -> String {
        format!("local:{}", self.model)
    }
}

/// C1 — the third adapter variant: the SAME llama.cpp server, but the action
/// space delivered as native `tools` instead of a GBNF grammar. The response
/// contract mirrors the OpenRouter adapter (`tool_calls` preferred, a JSON
/// envelope in `content` as fallback); the kernel re-validates either way.
pub struct LocalNativeAdapter {
    client: reqwest::Client,
    base_url: String,
    model: String,
    tuning: AdapterTuning,
}

impl LocalNativeAdapter {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_tuning(base_url, model, AdapterTuning::default())
    }

    pub fn with_tuning(
        base_url: impl Into<String>,
        model: impl Into<String>,
        tuning: AdapterTuning,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: normalize_base(base_url.into()),
            model: model.into(),
            tuning,
        }
    }

    fn tools_payload(schema: &ActionSchema) -> Vec<serde_json::Value> {
        schema
            .actions
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": a.name,
                        "description": a.description,
                        "parameters": a.parameters,
                    }
                })
            })
            .collect()
    }
}

#[async_trait]
impl PolicyOracle for LocalNativeAdapter {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let limit = self.tuning.step_result_limit.unwrap_or(600);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": prompt::system_prompt_native(schema, context)},
                {"role": "user", "content": prompt::user_prompt_with_limit(context, limit)},
            ],
            "tools": Self::tools_payload(schema),
            "tool_choice": "auto",
            "temperature": 0.0,
            "max_tokens": self.tuning.max_tokens.unwrap_or(1024),
        });
        if let Some(thinking) = self.tuning.thinking {
            body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": thinking });
        }

        let res = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| PolicyError::Backend(format!("llama.cpp server unreachable: {e}")))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            return Err(PolicyError::Backend(format!(
                "llama.cpp server returned {status}: {text}"
            )));
        }

        let parsed: ChatResponse = res
            .json()
            .await
            .map_err(|e| PolicyError::Malformed(format!("bad response json: {e}")))?;
        let usage = parsed.usage.as_ref().map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            calls: 1,
        });
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| PolicyError::Malformed("no choices in response".into()))?;
        let finish_reason = choice.finish_reason.clone();
        let message = choice.message;

        // Preferred path: a native tool call.
        if let Some(call) = message.tool_calls.as_ref().and_then(|t| t.first()) {
            let arguments: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .map_err(|e| {
                    PolicyError::Malformed(format!(
                        "tool call arguments are not valid JSON: {e}: {}",
                        call.function.arguments
                    ))
                })?;
            return Ok(Proposal {
                action: ProposedAction {
                    action: call.function.name.clone(),
                    arguments,
                    rationale: None,
                },
                usage,
            });
        }

        // Fallback: a JSON envelope in content.
        match message.content.as_deref() {
            Some(c) if !c.trim().is_empty() => {
                let action = prompt::parse_envelope(c).map_err(|e| {
                    PolicyError::Malformed(format!(
                        "no tool call and content is not an envelope: {e}"
                    ))
                })?;
                Ok(Proposal { action, usage })
            }
            _ => Err(empty_content_error(&message, finish_reason.as_deref())),
        }
    }

    fn call_bound(&self, context: &PolicyContext, schema: &ActionSchema) -> CallBound {
        let limit = self.tuning.step_result_limit.unwrap_or(600);
        crate::chat_call_bound(
            &[
                &prompt::system_prompt_native(schema, context),
                &prompt::user_prompt_with_limit(context, limit),
            ],
            Some(&Self::tools_payload(schema)),
            self.tuning.max_tokens.unwrap_or(1024),
        )
    }

    fn describe(&self) -> String {
        format!("local-native:{}", self.model)
    }
}
