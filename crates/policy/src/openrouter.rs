//! The `openrouter` adapter: one HTTP adapter reaching many frontier models
//! through OpenRouter's unified chat-completions API, selected by model
//! string. Satisfies the adapter contract with native tool-calling; the
//! kernel still re-validates on top.
//!
//! Request/response shapes verified against the live OpenRouter docs on
//! 2026-07-18 (see docs/BUILD-EVIDENCE.md): POST {base}/v1/chat/completions,
//! `Authorization: Bearer`, `tools[].function`, response
//! `message.tool_calls[].function.arguments` as a JSON-*encoded string*.

use async_trait::async_trait;
use serde::Deserialize;

use kernel_core::{
    ActionSchema, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, Usage,
};

use crate::prompt;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";

pub struct OpenRouterAdapter {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
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
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
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

impl OpenRouterAdapter {
    /// `api_key` must come from the environment (`OPENROUTER_API_KEY`);
    /// see [`crate::build_oracle`]. Never hardcode or persist it.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            // The request path below appends `/v1/chat/completions`. OpenRouter's
            // documented base is `.../api/v1`, so a base that already carries a
            // trailing `/v1` would yield `/v1/v1/...` → 404 on every model. Strip
            // it here so both `.../api` and `.../api/v1` resolve identically.
            base_url: {
                let raw = base_url.into();
                let b = raw.trim_end_matches('/');
                b.strip_suffix("/v1").unwrap_or(b).to_string()
            },
            model: model.into(),
            api_key: api_key.into(),
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
impl PolicyOracle for OpenRouterAdapter {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": prompt::system_prompt_native(schema, context)},
                {"role": "user", "content": prompt::user_prompt(context)},
            ],
            "tools": Self::tools_payload(schema),
            "tool_choice": "auto",
            "temperature": 0.0,
            "max_tokens": 1024,
        });

        let res = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            // Optional attribution headers; canonical + legacy alias.
            .header("HTTP-Referer", "https://localhost")
            .header("X-OpenRouter-Title", "kelter")
            .header("X-Title", "kelter")
            .json(&body)
            .send()
            .await
            .map_err(|e| PolicyError::Backend(format!("openrouter unreachable: {e}")))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            return Err(PolicyError::Backend(format!(
                "openrouter returned {status}: {text}"
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
        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| PolicyError::Malformed("no choices in response".into()))?;

        // Preferred path: a native tool call.
        if let Some(call) = message.tool_calls.and_then(|mut t| {
            if t.is_empty() {
                None
            } else {
                Some(t.remove(0))
            }
        }) {
            let arguments: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .map_err(|e| {
                    PolicyError::Malformed(format!(
                        "tool call arguments are not valid JSON: {e}: {}",
                        call.function.arguments
                    ))
                })?;
            return Ok(Proposal {
                action: ProposedAction {
                    action: call.function.name,
                    arguments,
                    rationale: None,
                },
                usage,
            });
        }

        // Fallback: some models answer with a JSON envelope in content.
        if let Some(content) = message.content.as_deref() {
            let action = prompt::parse_envelope(content).map_err(|e| {
                PolicyError::Malformed(format!("no tool call and content is not an envelope: {e}"))
            })?;
            return Ok(Proposal { action, usage });
        }

        Err(PolicyError::Malformed(
            "response contained neither tool_calls nor content".into(),
        ))
    }

    fn describe(&self) -> String {
        format!("openrouter:{}", self.model)
    }
}
