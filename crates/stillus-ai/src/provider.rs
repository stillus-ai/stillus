// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Provider adapters are the sole owners of wire formats and model capabilities.
use crate::{AiError, AiModel, AiProvider, ApiKey, CatalogTransport, journal::RequestJournal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InputItem {
    User(String),
    Assistant {
        text: String,
        state: Option<Value>,
    },
    ToolResult {
        call_id: String,
        name: String,
        output: Value,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Generation {
    pub content_policy: crate::journal::ContentPolicy,
    pub model: String,
    pub effort: Option<crate::AiEffort>,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolDefinition>,
    pub summary: bool,
    pub context_summary: Option<String>,
    pub chat: String,
    pub run: String,
    pub step: u32,
    pub max_output: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenerationResult {
    pub text: String,
    pub calls: Vec<ToolInvocation>,
    pub state: Option<Value>,
    pub usage: Usage,
    pub request_id: String,
}
#[derive(Clone, Debug)]
pub enum GenerationEvent {
    Text(String),
    Tool(ToolInvocation),
    Usage(Usage),
    Completed(GenerationResult),
}
#[derive(Clone, Copy, Debug)]
pub struct ModelCapabilities {
    pub generation: bool,
    pub tools: bool,
    pub context_tokens: u64,
    pub output_tokens: u64,
}
impl ModelCapabilities {
    // UTF-8 bytes conservatively overestimate normal prose without a tokenizer.
    pub fn estimate(&self, input: &[InputItem]) -> u64 {
        serde_json::to_vec(input)
            .map(|b| b.len() as u64)
            .unwrap_or(u64::MAX)
    }
    pub fn needs_summary_with_context(
        &self,
        input: &[InputItem],
        tools: &[ToolDefinition],
        summary: Option<&str>,
    ) -> bool {
        self.estimate(input)
            .saturating_add(
                serde_json::to_vec(tools)
                    .map(|v| v.len() as u64)
                    .unwrap_or(u64::MAX),
            )
            .saturating_add(summary.map(|s| s.len() as u64).unwrap_or(0))
            .saturating_add(self.output_tokens)
            .saturating_add(2048)
            > self.context_tokens.saturating_mul(80) / 100
    }
    pub fn needs_summary(&self, input: &[InputItem]) -> bool {
        self.estimate(input).saturating_add(self.output_tokens)
            > self.context_tokens.saturating_mul(80) / 100
    }
}
#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release)
    }
    pub fn check(&self) -> Result<(), AiError> {
        if self.0.load(Ordering::Acquire) {
            Err(AiError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub trait AiProviderEngine: Send + Sync {
    fn id(&self) -> AiProvider;
    fn name(&self) -> &str;
    fn recognizes_key(&self, key: &str) -> bool;
    fn default_model(&self) -> &str;
    fn catalog(
        &self,
        key: &ApiKey,
        journal: &dyn RequestJournal,
        operation: &str,
    ) -> Result<Vec<AiModel>, AiError>;
    fn capabilities(&self, model: &str) -> Result<ModelCapabilities, AiError>;
    fn generate(
        &self,
        request: &Generation,
        key: &ApiKey,
        journal: &dyn RequestJournal,
        cancel: &Cancellation,
        emit: &mut dyn FnMut(GenerationEvent),
    ) -> Result<GenerationResult, AiError>;
}
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    engines: BTreeMap<AiProvider, Arc<dyn AiProviderEngine>>,
}
impl ProviderRegistry {
    pub fn standard() -> Self {
        let mut r = Self::default();
        r.register(Arc::new(crate::openai::OpenAiEngine::default()))
            .expect("unique provider");
        r.register(Arc::new(AnthropicCatalog))
            .expect("unique provider");
        r
    }
    pub fn register(&mut self, engine: Arc<dyn AiProviderEngine>) -> Result<(), AiError> {
        let id = engine.id();
        if id.id().is_empty() || self.engines.contains_key(&id) {
            return Err(AiError::Response);
        }
        self.engines.insert(id, engine);
        Ok(())
    }
    pub fn get(&self, id: &AiProvider) -> Option<Arc<dyn AiProviderEngine>> {
        self.engines.get(id).cloned()
    }
    pub fn providers(&self) -> impl Iterator<Item = &Arc<dyn AiProviderEngine>> {
        self.engines.values()
    }
    pub fn detect(&self, key: &str) -> Option<AiProvider> {
        self.providers()
            .find(|p| p.recognizes_key(key))
            .map(|p| p.id())
    }
}
pub struct RegistryCatalogTransport;
impl CatalogTransport for RegistryCatalogTransport {
    fn list(&self, _: AiProvider, _: &ApiKey) -> Result<Vec<AiModel>, AiError> {
        Err(AiError::Journal)
    }
    fn list_recorded(
        &self,
        provider: AiProvider,
        key: &ApiKey,
        journal: &dyn RequestJournal,
        operation: &str,
    ) -> Result<Vec<AiModel>, AiError> {
        ProviderRegistry::standard()
            .get(&provider)
            .ok_or(AiError::Unsupported)?
            .catalog(key, journal, operation)
    }
}
pub(crate) fn key_shape(key: &str) -> bool {
    (24..=4096).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
struct AnthropicCatalog;
impl AiProviderEngine for AnthropicCatalog {
    fn id(&self) -> AiProvider {
        AiProvider::Anthropic
    }
    fn name(&self) -> &str {
        "Anthropic"
    }
    fn default_model(&self) -> &str {
        "claude-sonnet-5"
    }
    fn recognizes_key(&self, key: &str) -> bool {
        key_shape(key) && key.starts_with("sk-ant-api")
    }
    fn catalog(
        &self,
        key: &ApiKey,
        journal: &dyn RequestJournal,
        operation: &str,
    ) -> Result<Vec<AiModel>, AiError> {
        crate::HttpsCatalogTransport.list_recorded(self.id(), key, journal, operation)
    }
    fn capabilities(&self, _: &str) -> Result<ModelCapabilities, AiError> {
        Ok(ModelCapabilities {
            generation: false,
            tools: false,
            context_tokens: 0,
            output_tokens: 0,
        })
    }
    fn generate(
        &self,
        _: &Generation,
        _: &ApiKey,
        _: &dyn RequestJournal,
        _: &Cancellation,
        _: &mut dyn FnMut(GenerationEvent),
    ) -> Result<GenerationResult, AiError> {
        Err(AiError::Unsupported)
    }
}

/// Retains the newest complete exchange and the whole current tool chain.
/// Returned prefix is summarized as data, never promoted to system instructions.
pub fn compression_split(input: &[InputItem]) -> Option<usize> {
    let users = input
        .iter()
        .enumerate()
        .filter_map(|(i, m)| matches!(m, InputItem::User(_)).then_some(i))
        .collect::<Vec<_>>();
    if users.len() < 3 {
        None
    } else {
        Some(users[users.len() - 2])
    }
}
