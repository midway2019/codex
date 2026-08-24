//! Artesia-aware ContextManager adapter.
//!
//! Implements the adapter pattern (similar to SGLang's RadixCache adapter):
//! `ArtesiaContextManager` wraps the real `ContextManager` and delegates all
//! operations to it (super() semantics), then queues Artesia SDK synchronization
//! operations to be flushed asynchronously.
//!
//! Usage: replace `ContextManager` with `ArtesiaContextManager` in `SessionState`.
//! All existing call-sites continue to work unchanged — same method signatures —
//! but now additionally synchronize state to the Artesia service.

use super::ContextManager;
use super::TotalTokenUsageBreakdown;
use crate::session::turn_context::TurnContext;
use artesia_client::models::Message;
use artesia_client::ArtesiaClient;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_output_truncation::TruncationPolicy;
use std::ops::Deref;

/// Artesia-aware wrapper around Codex's `ContextManager`.
///
/// Every public method mirrors `ContextManager`'s interface:
/// 1. Delegates to `self.inner.<method>(...)` — original logic executes in full.
/// 2. Immediately calls the Artesia SDK service (synchronous HTTP) to sync state.
///
/// When `enabled=false` (passthrough mode), no HTTP calls are made.
#[derive(Debug)]
pub(crate) struct ArtesiaContextManager {
    /// The original ContextManager — all operations delegate here first.
    inner: ContextManager,

    /// Artesia SDK client — synchronous blocking HTTP calls.
    client: ArtesiaClient,

    /// Context ID assigned for this session on the Artesia service.
    context_id: String,

    /// Whether Artesia sync is enabled. If false, pure passthrough.
    /// Cloned copies always have enabled=false to prevent duplicate Artesia calls.
    enabled: bool,

    /// Whether virtual prefix has been synced to Artesia (only once per session).
    virtual_prefix_synced: bool,

    /// Whether every inference in this context is one-off.
    one_off: bool,
}

impl Clone for ArtesiaContextManager {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            client: self.client.clone(),
            context_id: self.context_id.clone(),
            // Clones are never the "owner" — only the original syncs to Artesia.
            enabled: false,
            virtual_prefix_synced: true,
            one_off: self.one_off,
        }
    }
}

impl ArtesiaContextManager {
    /// Create a new adapter wrapping a fresh ContextManager.
    /// `context_id` is the ID to use for the local context.
    ///
    /// Uses `ArtesiaClient::local()` — all operations run in-process via C++ FFI,
    /// no HTTP server needed.
    pub(crate) fn new(context_id: String) -> Self {
        let client = ArtesiaClient::local();
        if let Err(e) = client.create_context(&context_id) {
            tracing::warn!("Artesia create_context failed: {e}");
        }
        Self {
            inner: ContextManager::new(),
            client,
            context_id,
            enabled: true,
            virtual_prefix_synced: false,
            one_off: false,
        }
    }

    /// Create an adapter in disabled/passthrough mode (no Artesia calls).
    /// Behaves identically to a bare ContextManager.
    pub(crate) fn passthrough() -> Self {
        Self {
            inner: ContextManager::new(),
            client: ArtesiaClient::local(),
            context_id: String::new(),
            enabled: false,
            virtual_prefix_synced: true,
            one_off: false,
        }
    }

    // ─── Delegated methods (super() + immediate Artesia sync) ───

    /// Record items into the context.
    /// Delegates to inner.record_items(), then immediately syncs to Artesia.
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy) where I: IntoIterator, I::Item: Deref<Target = ResponseItem> {
        let items_vec: Vec<_> = items.into_iter().collect();
        let count_before = self.inner.raw_items().len();

        self.inner.record_items(items_vec.iter().map(|i| i.deref()), policy);

        if self.enabled {
            let count_after = self.inner.raw_items().len();
            if count_after > count_before {
                let messages: Vec<Message> = self.inner.raw_items()[count_before..count_after]
                    .iter()
                    .map(|item| response_item_to_message(item))
                    .collect();
                for msg in &messages {
                    if let Err(e) = self.client.append_message(&self.context_id, msg) {
                        tracing::warn!("Artesia append_message failed: {e}");
                    }
                }
            }
        }
    }

    /// Remove the first (oldest) item.
    /// Delegates to inner.remove_first_item(), then syncs to Artesia.
    pub(crate) fn remove_first_item(&mut self) {
        self.inner.remove_first_item();

        if self.enabled {
            if let Err(e) = self.client.remove_message(&self.context_id, 0) {
                tracing::warn!("Artesia remove_message(0) failed: {e}");
            }
        }
    }

    /// Full replace of the context (e.g., after compaction).
    /// Delegates to inner.replace(), then syncs full replacement to Artesia.
    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        self.inner.replace(items);

        if self.enabled {
            let messages: Vec<Message> = self
                .inner
                .raw_items()
                .iter()
                .map(|item| response_item_to_message(item))
                .collect();
            if let Err(e) = self.client.replace_all_messages(&self.context_id, &messages) {
                tracing::warn!("Artesia replace_all_messages failed: {e}");
            }
        }
    }

    /// Drop the last N user turns.
    /// Delegates to inner.drop_last_n_user_turns(), then syncs via replace_all.
    pub(crate) fn drop_last_n_user_turns(&mut self, num_turns: u32) {
        self.inner.drop_last_n_user_turns(num_turns);

        if self.enabled {
            let messages: Vec<Message> = self
                .inner
                .raw_items()
                .iter()
                .map(|item| response_item_to_message(item))
                .collect();
            if let Err(e) = self.client.replace_all_messages(&self.context_id, &messages) {
                tracing::warn!("Artesia replace_all after drop_turns failed: {e}");
            }
        }
    }

    // ─── Pure delegation (read-only, no Artesia side-effects) ───

    pub(crate) fn for_prompt(mut self, input_modalities: &[codex_protocol::openai_models::InputModality]) -> Vec<ResponseItem> {
        // Disable Artesia cleanup since this is a clone consumed for prompt building
        self.enabled = false;
        let inner = std::mem::replace(&mut self.inner, ContextManager::new());
        inner.for_prompt(input_modalities)
    }

    pub(crate) fn raw_items(&self) -> &[ResponseItem] {
        self.inner.raw_items()
    }

    pub(crate) fn into_raw_items(self) -> Vec<ResponseItem> {
        self.inner.into_raw_items()
    }

    pub(crate) fn history_version(&self) -> u64 {
        self.inner.history_version()
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.inner.token_info()
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.inner.set_token_info(info);
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.inner.set_reference_context_item(item);
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.inner.reference_context_item()
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.inner.set_token_usage_full(context_window);
    }

    pub(crate) fn update_token_info(&mut self, usage: &TokenUsage, model_context_window: Option<i64>) {
        self.inner.update_token_info(usage, model_context_window);
    }

    pub(crate) fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        self.inner.estimate_token_count(turn_context)
    }

    pub(crate) fn estimate_token_count_with_base_instructions(&self, base_instructions: &BaseInstructions) -> Option<i64> {
        self.inner.estimate_token_count_with_base_instructions(base_instructions)
    }

    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        self.inner.replace_last_turn_images(placeholder)
    }

    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        self.inner.get_total_token_usage(server_reasoning_included)
    }

    pub(crate) fn get_total_token_usage_breakdown(&self) -> TotalTokenUsageBreakdown {
        self.inner.get_total_token_usage_breakdown()
    }

    // ─── Artesia-specific methods ───

    pub(crate) fn context_id(&self) -> Option<String> {
        self.enabled.then(|| self.context_id.clone())
    }

    pub(crate) fn mark_fork_from(&self, parent_context_id: &str) {
        if self.enabled
            && let Err(e) = self
                .client
                .fork_context(parent_context_id, &self.context_id)
        {
            tracing::warn!("Artesia fork_context failed: {e}");
        }
    }

    /// Sync the virtual prefix (base instructions + tool descriptions) to Artesia.
    ///
    /// Virtual prefix messages are not part of `raw_items()` — they are injected
    /// dynamically at prompt construction time. This method informs ContextCake
    /// about these implicit prefix messages so it can correctly account for their
    /// KV Cache entries (pinned, never evicted).
    ///
    /// Should be called once per session, typically on the first turn when
    /// base_instructions and tools become available.
    pub(crate) fn sync_virtual_prefix(&mut self, instructions: &str, tools_description: &str) {
        if !self.enabled || self.virtual_prefix_synced || self.context_id.is_empty() {
            return;
        }

        self.virtual_prefix_synced = true;

        match self.client.set_virtual_prefix(
            &self.context_id,
            instructions,
            tools_description,
            "", // output_schema — empty for now
        ) {
            Ok(count) => {
                tracing::info!("Artesia virtual prefix synced: {count} prefix messages");
            }
            Err(e) => {
                tracing::warn!("Artesia set_virtual_prefix failed: {e}");
            }
        }
    }

    /// Mark all messages in this clone as one-off on the Artesia service.
    ///
    /// Called on a cloned history used for compaction, after `record_items`
    /// but before `for_prompt`. Tells ContextCake that the KV Cache entries
    /// produced by this inference are ephemeral and should not be retained.
    ///
    /// Uses a compact-specific context ID (`{main_context_id}:compact`) to
    /// distinguish from the main session's context.
    pub(crate) fn mark_one_off(&self) {
        if self.context_id.is_empty() {
            return;
        }

        let compact_context_id = format!("{}:compact", self.context_id);
        // Include the virtual prefix (1 message: base_instructions + tools combined)
        // in the count, since compact inference also produces KV Cache entries for it.
        let virtual_prefix_count = 1;
        let message_count = virtual_prefix_count + self.inner.raw_items().len();

        if let Err(e) = self.client.mark_all_one_off(&compact_context_id, message_count) {
            tracing::warn!("Artesia mark_one_off failed: {e}");
        }
    }

    pub(crate) fn set_one_off(&mut self) {
        self.one_off = true;
    }

    /// Mark this session's current messages as one-off before inference.
    pub(crate) fn mark_current_one_off(&self) {
        if !self.enabled || !self.one_off {
            return;
        }
        if let Err(e) = self
            .client
            .mark_all_one_off(&self.context_id, 1 + self.inner.raw_items().len())
        {
            tracing::warn!("Artesia mark_one_off failed: {e}");
        }
    }

    /// Explicitly suspend the Artesia context, releasing KV Cache resources.
    /// Called when the session's turn loop ends (no more LLM requests).
    pub(crate) fn suspend(&mut self) {
        if self.enabled && !self.context_id.is_empty() {
            tracing::info!("Artesia suspending context_id={}", self.context_id);
            if let Err(e) = self.client.delete_context(&self.context_id) {
                tracing::warn!("Artesia suspend (delete_context) failed: {e}");
            }
            self.enabled = false;
        }
    }
}


// ─── Helpers ───

/// Convert a Codex `ResponseItem` into an Artesia `Message`.
///
/// All conversation messages are marked as `Normal` lifecycle. Only the
/// virtual prefix (instructions + tools) is pinned, and that is handled
/// separately by `sync_virtual_prefix` on the SDK side.
fn response_item_to_message(item: &ResponseItem) -> Message {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let content_text = content
                .iter()
                .filter_map(|c| {
                    if let codex_protocol::models::ContentItem::InputText { text } = c {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            Message::new(role.to_string(), content_text)
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => Message::new("tool_call", format!("{}({})", name, arguments)),
        ResponseItem::FunctionCallOutput { output, .. } => {
            let text = output.body.to_text().unwrap_or_default();
            Message::new("tool_output", text)
        }
        ResponseItem::Reasoning {
            encrypted_content, ..
        } => Message::new(
            "reasoning",
            encrypted_content.clone().unwrap_or_default(),
        ),
        _ => Message::new("other", ""),
    }
}
