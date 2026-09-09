// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Provider-independent, credential-free request records. Persistence belongs to the host.
use crate::{AiError, AiProvider, ApiKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentPolicy {
    Public,
    Protected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    Pending,
    Success,
    Error,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub version: u32,
    pub id: String,
    pub operation: String,
    pub started_ms: u64,
    pub provider: AiProvider,
    pub purpose: String,
    pub endpoint: String,
    pub parameters: Value,
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub http_status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub status: RequestStatus,
    pub error: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tool_call: Option<String>,
    pub content_policy: ContentPolicy,
    #[serde(default)]
    pub chat: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub step: Option<u32>,
}

impl RequestRecord {
    pub fn catalog(provider: AiProvider, operation: &str, cursor: Option<&str>) -> Self {
        Self {
            version: 1,
            id: String::new(),
            operation: operation.into(),
            started_ms: now_ms(),
            provider: provider.clone(),
            purpose: "models/list".into(),
            endpoint: match provider {
                AiProvider::Other(_) => "",
                AiProvider::OpenAi => "https://api.openai.com/v1/models",
                AiProvider::Anthropic => "https://api.anthropic.com/v1/models",
            }
            .into(),
            parameters: match provider {
                AiProvider::Other(_) | AiProvider::OpenAi => serde_json::json!({}),
                AiProvider::Anthropic => serde_json::json!({"limit": 1000, "after_id": cursor}),
            },
            request: None,
            response: None,
            http_status: None,
            duration_ms: None,
            status: RequestStatus::Pending,
            error: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            tool_call: None,
            content_policy: ContentPolicy::Public,
            chat: None,
            run: None,
            step: None,
        }
    }

    /// Called at both persistence boundaries, including for linked tool results.
    pub fn enforce_content_policy(&mut self) {
        if self.content_policy == ContentPolicy::Protected {
            self.request = None;
            self.response = None;
            self.parameters = Value::Null;
            self.error = None;
        }
    }
}

pub trait RequestJournal: Send + Sync {
    /// Reserves bounded completion storage and durably records intent before any network I/O.
    fn begin(&self, record: RequestRecord) -> Result<RequestRecord, AiError>;
    /// The host must retain a failed completion and block begin until it is persisted.
    fn complete(&self, record: RequestRecord);
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// No response text or transport error is formatted until credentials have been removed.
pub fn safe_response(bytes: &[u8], key: &ApiKey) -> Value {
    fn scrub(value: &mut Value, key: &str) {
        match value {
            Value::String(text) => *text = text.replace(key, "[redacted]"),
            Value::Array(values) => values.iter_mut().for_each(|v| scrub(v, key)),
            Value::Object(values) => {
                let old = std::mem::take(values);
                for (name, mut value) in old {
                    if matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key" | "api_key" | "password"
                    ) {
                        value = Value::String("[redacted]".into());
                    } else {
                        scrub(&mut value, key);
                    }
                    values.insert(name.replace(key, "[redacted]"), value);
                }
            }
            _ => {}
        }
    }
    let mut value = serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()));
    scrub(&mut value, key.expose());
    value
}
