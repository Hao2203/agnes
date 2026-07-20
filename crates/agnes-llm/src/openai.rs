use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider, Role};
use serde::Serialize;

pub struct OpenAiCompatProvider {
    model: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct WireBody<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<WireMessage<'a>>,
}

impl OpenAiCompatProvider {
    pub fn new(model: String, api_key: String, base_url: String, client: reqwest::Client) -> Self {
        // Normalize: strip any trailing slash.
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            model,
            api_key,
            base_url,
            client,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Build the JSON body string. Exposed for shape tests.
    pub fn build_body(&self, req: &CompletionRequest) -> String {
        let mut msgs: Vec<WireMessage> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            msgs.push(WireMessage {
                role: "system",
                content: sys,
            });
        }
        for m in &req.messages {
            msgs.push(WireMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            });
        }
        serde_json::to_string(&WireBody {
            model: &self.model,
            max_tokens: req.max_tokens,
            messages: msgs,
        })
        .expect("serialize openai body")
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(self.endpoint())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: text,
            });
        }
        // { "choices": [ { "message": { "content": "..." } } ] }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Deserialize(format!("{e}: body was {text}")))?;
        let content = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                LlmError::Deserialize(format!("no choices[0].message.content in response: {text}"))
            })?;
        Ok(content.to_string())
    }
}
