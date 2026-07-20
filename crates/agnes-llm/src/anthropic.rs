use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider, Role};
use serde::Serialize;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    model: String,
    api_key: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<WireMessage<'a>>,
}

impl AnthropicProvider {
    pub fn new(model: String, api_key: String, client: reqwest::Client) -> Self {
        Self {
            model,
            api_key,
            client,
        }
    }

    /// Build the JSON body string. Exposed for shape tests.
    pub fn build_body(&self, req: &CompletionRequest) -> String {
        let msgs: Vec<WireMessage> = req
            .messages
            .iter()
            .map(|m| WireMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();
        let body = WireBody {
            model: &self.model,
            max_tokens: req.max_tokens,
            system: req.system.as_deref(),
            messages: msgs,
        };
        serde_json::to_string(&body).expect("serialize anthropic body")
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(ANTHROPIC_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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
        // Response shape: { "content": [ { "type": "text", "text": "..." }, ... ] }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Deserialize(format!("{e}: body was {text}")))?;
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| LlmError::Deserialize("no `content` array in response".into()))?;
        let mut out = String::new();
        for part in content {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(s);
                }
            }
        }
        Ok(out)
    }
}
