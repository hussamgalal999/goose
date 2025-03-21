/// A truncate agent that truncates the conversation history when it exceeds the model's context limit
/// It makes no attempt to handle context limits, and cannot read resources
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::{debug, error, instrument, warn};

use super::detect_read_only_tools;
use super::Agent;
use crate::agents::capabilities::Capabilities;
use crate::agents::extension::{ExtensionConfig, ExtensionResult};
use crate::config::Config;
use crate::message::{Message, ToolRequest};
use crate::providers::base::Provider;
use crate::providers::base::ProviderUsage;
use crate::providers::errors::ProviderError;
use crate::register_agent;
use crate::token_counter::TokenCounter;
use crate::truncate::{truncate_messages, OldestFirstTruncation};
use indoc::indoc;
use mcp_core::{tool::Tool, Content};
use serde_json::{json, Value};

const MAX_TRUNCATION_ATTEMPTS: usize = 3;
const ESTIMATE_FACTOR_DECAY: f32 = 0.9;

/// Truncate implementation of an Agent
pub struct TruncateAgent {
    capabilities: Mutex<Capabilities>,
    token_counter: TokenCounter,
    confirmation_tx: mpsc::Sender<(String, bool)>, // (request_id, confirmed)
    confirmation_rx: Mutex<mpsc::Receiver<(String, bool)>>,
}

impl TruncateAgent {
    pub fn new(provider: Box<dyn Provider>) -> Self {
        let token_counter = TokenCounter::new(provider.get_model_config().tokenizer_name());
        // Create channel with buffer size 32 (adjust if needed)
        let (tx, rx) = mpsc::channel(32);

        Self {
            capabilities: Mutex::new(Capabilities::new(provider)),
            token_counter,
            confirmation_tx: tx,
            confirmation_rx: Mutex::new(rx),
        }
    }

    /// Truncates the messages to fit within the model's context window
    /// Ensures the last message is a user message and removes tool call-response pairs
    async fn truncate_messages(
        &self,
        messages: &mut Vec<Message>,
        estimate_factor: f32,
        system_prompt: &str,
        tools: &mut Vec<Tool>,
    ) -> anyhow::Result<()> {
        // Model's actual context limit
        let context_limit = self
            .capabilities
            .lock()
            .await
            .provider()
            .get_model_config()
            .context_limit();

        // Our conservative estimate of the **target** context limit
        // Our token count is an estimate since model providers often don't provide the tokenizer (eg. Claude)
        let context_limit = (context_limit as f32 * estimate_factor) as usize;

        // Take into account the system prompt, and our tools input and subtract that from the
        // remaining context limit
        let system_prompt_token_count = self.token_counter.count_tokens(system_prompt);
        let tools_token_count = self.token_counter.count_tokens_for_tools(tools.as_slice());

        // Check if system prompt + tools exceed our context limit
        let remaining_tokens = context_limit
            .checked_sub(system_prompt_token_count)
            .and_then(|remaining| remaining.checked_sub(tools_token_count))
            .ok_or_else(|| {
                anyhow::anyhow!("System prompt and tools exceed estimated context limit")
            })?;

        let context_limit = remaining_tokens;

        // Calculate current token count of each message, use count_chat_tokens to ensure we
        // capture the full content of the message, include ToolRequests and ToolResponses
        let mut token_counts: Vec<usize> = messages
            .iter()
            .map(|msg| {
                self.token_counter
                    .count_chat_tokens("", std::slice::from_ref(msg), &[])
            })
            .collect();

        truncate_messages(
            messages,
            &mut token_counts,
            context_limit,
            &OldestFirstTruncation,
        )
    }
}

#[async_trait]
impl Agent for TruncateAgent {
    async fn add_extension(&mut self, extension: ExtensionConfig) -> ExtensionResult<()> {
        let mut capabilities = self.capabilities.lock().await;
        capabilities.add_extension(extension).await
    }

    async fn remove_extension(&mut self, name: &str) {
        let mut capabilities = self.capabilities.lock().await;
        capabilities
            .remove_extension(name)
            .await
            .expect("Failed to remove extension");
    }

    async fn list_extensions(&self) -> Vec<String> {
        let capabilities = self.capabilities.lock().await;
        capabilities
            .list_extensions()
            .await
            .expect("Failed to list extensions")
    }

    async fn passthrough(&self, _extension: &str, _request: Value) -> ExtensionResult<Value> {
        // TODO implement
        Ok(Value::Null)
    }

    /// Handle a confirmation response for a tool request
    async fn handle_confirmation(&self, request_id: String, confirmed: bool) {
        if let Err(e) = self.confirmation_tx.send((request_id, confirmed)).await {
            error!("Failed to send confirmation: {}", e);
        }
    }

    #[instrument(skip(self, messages), fields(user_message))]
    async fn reply(
        &self,
        messages: &[Message],
    ) -> anyhow::Result<BoxStream<'_, anyhow::Result<Message>>> {
        let mut messages = messages.to_vec();
        let reply_span = tracing::Span::current();
        let mut capabilities = self.capabilities.lock().await;
        let mut tools = capabilities.get_prefixed_tools().await?;
        let mut truncation_attempt: usize = 0;

        // Load settings from config
        let config = Config::global();
        let goose_mode = config.get("GOOSE_MODE").unwrap_or("auto".to_string());

        // we add in the 2 resource tools if any extensions support resources
        // TODO: make sure there is no collision with another extension's tool name
        let read_resource_tool = Tool::new(
            "platform__read_resource".to_string(),
            indoc! {r#"
                Read a resource from an extension.

                Resources allow extensions to share data that provide context to LLMs, such as
                files, database schemas, or application-specific information. This tool searches for the
                resource URI in the provided extension, and reads in the resource content. If no extension
                is provided, the tool will search all extensions for the resource.
            "#}.to_string(),
            json!({
                "type": "object",
                "required": ["uri"],
                "properties": {
                    "uri": {"type": "string", "description": "Resource URI"},
                    "extension_name": {"type": "string", "description": "Optional extension name"}
                }
            }),
        );

        let list_resources_tool = Tool::new(
            "platform__list_resources".to_string(),
            indoc! {r#"
                List resources from an extension(s).

                Resources allow extensions to share data that provide context to LLMs, such as
                files, database schemas, or application-specific information. This tool lists resources
                in the provided extension, and returns a list for the user to browse. If no extension
                is provided, the tool will search all extensions for the resource.
            "#}.to_string(),
            json!({
                "type": "object",
                "properties": {
                    "extension_name": {"type": "string", "description": "Optional extension name"}
                }
            }),
        );

        if capabilities.supports_resources() {
            tools.push(read_resource_tool);
            tools.push(list_resources_tool);
        }

        let system_prompt = capabilities.get_system_prompt().await;

        // Set the user_message field in the span instead of creating a new event
        if let Some(content) = messages
            .last()
            .and_then(|msg| msg.content.first())
            .and_then(|c| c.as_text())
        {
            debug!("user_message" = &content);
        }

        Ok(Box::pin(async_stream::try_stream! {
            let _reply_guard = reply_span.enter();
            loop {
                match capabilities.provider().complete(
                    &system_prompt,
                    &messages,
                    &tools,
                ).await {
                    Ok((response, usage)) => {
                        capabilities.record_usage(usage).await;

                        // Reset truncation attempt
                        truncation_attempt = 0;

                        // Yield the assistant's response
                        yield response.clone();

                        tokio::task::yield_now().await;

                        // First collect any tool requests
                        let tool_requests: Vec<&ToolRequest> = response.content
                            .iter()
                            .filter_map(|content| content.as_tool_request())
                            .collect();

                        if tool_requests.is_empty() {
                            break;
                        }

                        // Process tool requests depending on goose_mode
                        let mut message_tool_response = Message::user();
                        // Clone goose_mode once before the match to avoid move issues
                        let mode = goose_mode.clone();
                        match mode.as_str() {
                            "approve" => {
                                // Process each tool request sequentially with confirmation
                                let read_only_tools = detect_read_only_tools(&capabilities, tool_requests.clone()).await;
                                for request in &tool_requests {
                                    if let Ok(tool_call) = request.tool_call.clone() {
                                        // Skip confirmation if the tool_call.name is in the read_only_tools list
                                        if read_only_tools.contains(&tool_call.name) {
                                            let output = capabilities.dispatch_tool_call(tool_call).await;
                                                    message_tool_response = message_tool_response.with_tool_response(
                                                        request.id.clone(),
                                                        output,
                                                    );
                                        } else {
                                            let confirmation = Message::user().with_tool_confirmation_request(
                                                request.id.clone(),
                                                tool_call.name.clone(),
                                                tool_call.arguments.clone(),
                                                Some("Goose would like to call the above tool. Allow? (y/n):".to_string()),
                                            );
                                            yield confirmation;

                                            // Wait for confirmation response through the channel
                                            let mut rx = self.confirmation_rx.lock().await;
                                            if let Some((req_id, confirmed)) = rx.recv().await {
                                                if req_id == request.id {
                                                    if confirmed {
                                                        // User approved - dispatch the tool call
                                                        let output = capabilities.dispatch_tool_call(tool_call).await;
                                                        message_tool_response = message_tool_response.with_tool_response(
                                                            request.id.clone(),
                                                            output,
                                                        );
                                                    } else {
                                                        // User declined - add declined response
                                                        message_tool_response = message_tool_response.with_tool_response(
                                                            request.id.clone(),
                                                            Ok(vec![Content::text("User declined to run this tool.")]),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            "chat" => {
                                // Skip all tool calls in chat mode
                                for request in &tool_requests {
                                    message_tool_response = message_tool_response.with_tool_response(
                                        request.id.clone(),
                                        Ok(vec![Content::text(
                                            "The following tool call was skipped in Goose chat mode. \
                                            In chat mode, you cannot run tool calls, instead, you can \
                                            only provide a detailed plan to the user. Provide an \
                                            explanation of the proposed tool call as if it were a plan. \
                                            Only if the user asks, provide a short explanation to the \
                                            user that they could consider running the tool above on \
                                            their own or with a different goose mode."
                                        )]),
                                    );
                                }
                            },
                            _ => {
                                if mode != "auto" {
                                    warn!("Unknown GOOSE_MODE: {mode:?}. Defaulting to 'auto' mode.");
                                }
                                // Process tool requests in parallel
                                let mut tool_futures = Vec::new();
                                for request in &tool_requests {
                                    if let Ok(tool_call) = request.tool_call.clone() {
                                        tool_futures.push(async {
                                            let output = capabilities.dispatch_tool_call(tool_call).await;
                                            (request.id.clone(), output)
                                        });
                                    }
                                }
                                // Wait for all tool calls to complete
                                let results = futures::future::join_all(tool_futures).await;
                                for (request_id, output) in results {
                                    message_tool_response = message_tool_response.with_tool_response(
                                        request_id,
                                        output,
                                    );
                                }
                            }
                        }

                        yield message_tool_response.clone();

                        messages.push(response);
                        messages.push(message_tool_response);
                    },
                    Err(ProviderError::ContextLengthExceeded(_)) => {
                        if truncation_attempt >= MAX_TRUNCATION_ATTEMPTS {
                            // Create an error message & terminate the stream
                            // the previous message would have been a user message (e.g. before any tool calls, this is just after the input message.
                            // at the start of a loop after a tool call, it would be after a tool_use assistant followed by a tool_result user)
                            yield Message::assistant().with_text("Error: Context length exceeds limits even after multiple attempts to truncate. Please start a new session with fresh context and try again.");
                            break;
                        }

                        truncation_attempt += 1;
                        warn!("Context length exceeded. Truncation Attempt: {}/{}.", truncation_attempt, MAX_TRUNCATION_ATTEMPTS);

                        // Decay the estimate factor as we make more truncation attempts
                        // Estimate factor decays like this over time: 0.9, 0.81, 0.729, ...
                        let estimate_factor: f32 = ESTIMATE_FACTOR_DECAY.powi(truncation_attempt as i32);

                        // release the lock before truncation to prevent deadlock
                        drop(capabilities);

                        if let Err(err) = self.truncate_messages(&mut messages, estimate_factor, &system_prompt, &mut tools).await {
                            yield Message::assistant().with_text(format!("Error: Unable to truncate messages to stay within context limit. \n\nRan into this error: {}.\n\nPlease start a new session with fresh context and try again.", err));
                            break;
                        }


                        // Re-acquire the lock
                        capabilities = self.capabilities.lock().await;

                        // Retry the loop after truncation
                        continue;
                    },
                    Err(e) => {
                        // Create an error message & terminate the stream
                        error!("Error: {}", e);
                        yield Message::assistant().with_text(format!("Ran into this error: {e}.\n\nPlease retry if you think this is a transient or recoverable error."));
                        break;
                    }
                }

                // Yield control back to the scheduler to prevent blocking
                tokio::task::yield_now().await;
            }
        }))
    }

    async fn usage(&self) -> Vec<ProviderUsage> {
        let capabilities = self.capabilities.lock().await;
        capabilities.get_usage().await
    }

    async fn extend_system_prompt(&mut self, extension: String) {
        let mut capabilities = self.capabilities.lock().await;
        capabilities.add_system_prompt_extension(extension);
    }

    async fn override_system_prompt(&mut self, template: String) {
        let mut capabilities = self.capabilities.lock().await;
        capabilities.set_system_prompt_override(template);
    }
}

register_agent!("truncate", TruncateAgent);
