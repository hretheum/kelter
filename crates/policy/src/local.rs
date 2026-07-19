//! The `local` adapter: llama.cpp server over HTTP with GBNF constrained
//! decoding. The action envelope is enforced by the *decoder* — a weak model
//! cannot emit a malformed tool call. This is the "weak but reliable" path.
//!
//! Verified against llama.cpp b10050: `/v1/chat/completions` accepts the
//! llama.cpp `grammar` extension field and applies the model's chat template
//! server-side (see docs/BUILD-EVIDENCE.md).

use async_trait::async_trait;
use serde::Deserialize;

use kernel_core::{ActionSchema, PolicyContext, PolicyError, PolicyOracle, Proposal, Usage};

use crate::gbnf;
use crate::prompt;

pub struct LocalAdapter {
    client: reqwest::Client,
    base_url: String,
    model: String,
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
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

impl LocalAdapter {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            // The request path appends `/v1/chat/completions`; a base that already
            // ends in `/v1` (e.g. Ollama's `.../11434/v1`) would double it →
            // `/v1/v1/...` → 404. Strip a trailing `/v1` so `.../8080` and
            // `.../11434/v1` both resolve correctly.
            base_url: {
                let raw = base_url.into();
                let b = raw.trim_end_matches('/');
                b.strip_suffix("/v1").unwrap_or(b).to_string()
            },
            model: model.into(),
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
        let body = serde_json::json!({
            // llama-server ignores the model name (it serves one model),
            // but the field keeps the request OpenAI-shaped.
            "model": self.model,
            "messages": [
                {"role": "system", "content": prompt::system_prompt(schema, context)},
                {"role": "user", "content": prompt::user_prompt(context)},
            ],
            // llama.cpp extension: GBNF grammar constraining the decoder.
            "grammar": grammar,
            "temperature": 0.0,
            "max_tokens": 512,
        });

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
        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .ok_or_else(|| PolicyError::Malformed("empty completion".into()))?;

        let action = prompt::parse_envelope(content).map_err(PolicyError::Malformed)?;
        Ok(Proposal { action, usage })
    }

    fn describe(&self) -> String {
        format!("local:{}", self.model)
    }
}
