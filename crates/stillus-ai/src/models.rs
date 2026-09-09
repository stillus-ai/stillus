// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::{AiEffort, AiModel, AiProvider};
use serde_json::Value;

/// Reviewed API capabilities, not inferred from a model's relative size.
/// Sources: developers.openai.com/api/docs/models and
/// platform.claude.com/docs/en/build-with-claude/effort (September 2026).
pub(crate) fn model(provider: AiProvider, raw: &Value) -> Option<AiModel> {
    let id = raw.get("id")?.as_str()?;
    if !valid_id(id) {
        return None;
    }
    let (name, efforts) = match provider {
        AiProvider::OpenAi => openai(id)?,
        AiProvider::Anthropic => {
            if !id.starts_with("claude-") {
                return None;
            }
            let capabilities = raw.pointer("/capabilities/effort");
            let efforts = if let Some(supported) = capabilities
                .and_then(|c| c.get("supported"))
                .and_then(Value::as_bool)
            {
                if supported {
                    let levels: Vec<_> = AiEffort::ALL
                        .into_iter()
                        .filter(|effort| {
                            capabilities
                                .and_then(|c| c.get(effort.name()))
                                .and_then(|c| c.get("supported"))
                                .and_then(Value::as_bool)
                                == Some(true)
                        })
                        .collect();
                    if levels.is_empty() {
                        return None;
                    }
                    levels
                } else {
                    vec![]
                }
            } else {
                anthropic(id)?
            };
            let name = raw
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|name| name.len() <= 120 && !name.chars().any(char::is_control))
                .unwrap_or(id)
                .to_owned();
            (name, efforts)
        }
    };
    Some(AiModel {
        id: id.to_owned(),
        name,
        efforts,
    })
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub(crate) fn is_snapshot(id: &str, alias: &str) -> bool {
    id.strip_prefix(alias).is_some_and(|suffix| {
        let bytes = suffix.as_bytes();
        bytes.len() == 11
            && bytes[0] == b'-'
            && bytes[5] == b'-'
            && bytes[8] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| matches!(i, 0 | 5 | 8) || b.is_ascii_digit())
    })
}

fn openai(id: &str) -> Option<(String, Vec<AiEffort>)> {
    use AiEffort::*;
    let current = [
        ("gpt-6-astra", "GPT-6 Astra"),
        ("gpt-5.6-luna", "GPT-5.6 Luna"),
        ("gpt-5.6-terra", "GPT-5.6 Terra"),
        ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ];
    for (alias, name) in current {
        if id == alias || is_snapshot(id, alias) {
            let levels = if alias == "gpt-6-astra" {
                vec![Low, Medium, High, Xhigh, Max]
            } else {
                vec![None, Low, Medium, High, Xhigh, Max]
            };
            let name = if id == alias {
                name.to_owned()
            } else {
                id.to_owned()
            };
            return Some((name, levels));
        }
    }
    for alias in [
        "gpt-4.1",
        "gpt-4.1-mini",
        "gpt-4.1-nano",
        "gpt-4o",
        "gpt-4o-mini",
    ] {
        if id == alias || is_snapshot(id, alias) {
            return Some((id.to_owned(), vec![]));
        }
    }
    Option::None
}

fn anthropic(id: &str) -> Option<Vec<AiEffort>> {
    use AiEffort::*;
    let alias = id.strip_suffix("-20251001").unwrap_or(id);
    if matches!(alias, "claude-haiku-4-5" | "claude-sonnet-4-5") {
        return Some(vec![]);
    }
    if id == "claude-opus-4-5" || id == "claude-opus-4-5-20251101" {
        return Some(vec![Low, Medium, High]);
    }
    if matches!(id, "claude-opus-4-6" | "claude-sonnet-4-6") {
        return Some(vec![Low, Medium, High, Max]);
    }
    if matches!(
        id,
        "claude-opus-4-7"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-sonnet-5"
            | "claude-fable-5"
            | "claude-fable-5-1"
            | "claude-mythos-5"
            | "claude-mythos-5-1"
    ) {
        return Some(vec![Low, Medium, High, Xhigh, Max]);
    }
    Option::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn catalog_excludes_unknown_and_non_text_models() {
        for id in [
            "gpt-image-1",
            "text-embedding-3-small",
            "gpt-future",
            "gpt-5.6-luna-audio",
            "gpt-5.6-luna-instruct",
        ] {
            assert!(model(AiProvider::OpenAi, &json!({"id": id})).is_none());
        }
        assert!(
            model(AiProvider::OpenAi, &json!({"id":"gpt-5.6-luna"}))
                .unwrap()
                .efforts
                .contains(&AiEffort::High)
        );
    }
    #[test]
    fn anthropic_capabilities_override_fallback() {
        let raw = json!({"id":"claude-future", "capabilities":{"effort":{"supported":true,"low":{"supported":true},"max":{"supported":false}}}});
        assert_eq!(
            model(AiProvider::Anthropic, &raw).unwrap().efforts,
            vec![AiEffort::Low]
        );
        assert!(model(AiProvider::Anthropic, &json!({"id":"claude-future"})).is_none());
    }
}
