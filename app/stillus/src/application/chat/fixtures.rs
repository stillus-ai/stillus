// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Offline wire fixtures, shared by headless and native UI acceptance tests.
use super::*;
use std::io::Read;
use stillus_ai::openai::{OpenAiEngine, ResponsesTransport, ToolNames};
pub(crate) fn registry() -> ProviderRegistry {
    let mut r = ProviderRegistry::default();
    r.register(Arc::new(OpenAiEngine::with_transport(Arc::new(Transport))))
        .unwrap();
    r
}
pub(crate) struct Transport;
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(v) = value.get(key) {
        return Some(v);
    }
    match value {
        Value::Object(fields) => fields.values().find_map(|v| find(v, key)),
        Value::Array(items) => items.iter().find_map(|v| find(v, key)),
        _ => None,
    }
}
impl ResponsesTransport for Transport {
    fn send(
        &self,
        _: &ApiKey,
        body: &[u8],
        consume: &mut dyn FnMut(u16, &mut dyn Read) -> Result<(), stillus_ai::AiError>,
    ) -> Result<(), stillus_ai::AiError> {
        let body: Value =
            serde_json::from_slice(body).map_err(|_| stillus_ai::AiError::Response)?;
        let names = ToolNames::new(
            &tools::list()
                .into_iter()
                .map(|t| ToolDefinition {
                    name: t.name.into(),
                    description: t.description.into(),
                    schema: t.input_schema,
                })
                .collect::<Vec<_>>(),
        )?;
        let input = body["input"]
            .as_array()
            .ok_or(stillus_ai::AiError::Response)?;
        let instruction = input
            .iter()
            .rfind(|i| i["role"] == "user")
            .and_then(|i| i["content"].as_str())
            .unwrap_or("");
        let outputs = input
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .filter_map(|i| serde_json::from_str::<Value>(i["output"].as_str()?).ok())
            .collect::<Vec<_>>();
        let task = instruction.contains("summary") || instruction.contains("итог");
        let mut text="Готово. Ответ сохранён в чате.\n\n- История остаётся локальной.\n- Можно продолжить разговор.\n\n```rust\nlet answer = 42;\n```".to_string();
        if instruction.contains("layout fixture") {
            text = format!(
                "{}\n\n[Long link](https://example.invalid/{})\n\n```text\n{}\n```\n\nОтвет сохранён. Конец длинного ответа.",
                "Длинный ответ должен переноситься внутри истории и не сдвигать элементы управления.\n\n".repeat(24),
                "longpath".repeat(100),
                "0123456789abcdef".repeat(120),
            );
        }
        let call = if instruction.contains("request limit") && outputs.len() < 22 {
            Some(("chats/list", json!({})))
        } else if task {
            match outputs.len() {
                0 => Some(("search/query", json!({"query":"Alpha"}))),
                1 => Some((
                    "notes/read",
                    json!({"id":find(&outputs[0],"id").and_then(Value::as_str).ok_or(stillus_ai::AiError::Response)?}),
                )),
                2 => Some(("notes/create", json!({"title":"AI Summary"}))),
                3 => Some((
                    "notes/read",
                    json!({"id":find(&outputs[2],"id").and_then(Value::as_str).ok_or(stillus_ai::AiError::Response)?}),
                )),
                4 => Some((
                    "notes/update",
                    json!({"id":find(&outputs[3],"id").and_then(Value::as_str).ok_or(stillus_ai::AiError::Response)?,"version":find(&outputs[3],"version").and_then(Value::as_str).ok_or(stillus_ai::AiError::Response)?,"start":0,"end":find(&outputs[3],"total_bytes").and_then(Value::as_u64).unwrap_or(0),"text":"AI Summary\n\nAlpha body: краткий итог исходной заметки.\n"}),
                )),
                _ => None,
            }
        } else {
            None
        };
        let output = if instruction.contains("tool limit") && outputs.is_empty() {
            Value::Array((0..60).map(|i|json!({"type":"function_call","name":names.wire("chats/create").unwrap(),"call_id":format!("fixture/batch/{i}"),"arguments":json!({"title":format!("Generated {i}")}).to_string()})).collect())
        } else if let Some((name, args)) = call {
            text = format!("Выполняю {name}…");
            json!([{"type":"function_call","name":names.wire(name)?,"call_id":format!("fixture/call/{}",outputs.len()),"arguments":args.to_string()}])
        } else {
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}])
        };
        let mut events = String::new();
        for part in text.chars().collect::<Vec<_>>().chunks(24) {
            let delta = part.iter().collect::<String>();
            events.push_str(&format!(
                "data: {}\n\n",
                json!({"type":"response.output_text.delta","delta":delta})
            ));
        }
        events.push_str(&format!("data: {}\n\n",json!({"type":"response.completed","response":{"output":output,"usage":{"input_tokens":123,"output_tokens":45}}})));
        consume(
            200,
            &mut Stream {
                bytes: std::io::Cursor::new(events.into_bytes()),
                gate: if matches!(
                    instruction,
                    "slow first" | "slow second" | "slow double click" | "slow stopped"
                ) {
                    std::env::var_os("STILLUS_TEST_CHAT_GATE").map(|path| {
                        (
                            PathBuf::from(path),
                            std::time::Instant::now() + Duration::from_secs(30),
                        )
                    })
                } else {
                    None
                },
                delay_ms: if instruction.contains("slow") {
                    400
                } else if instruction.contains("layout fixture") {
                    10
                } else {
                    0
                },
            },
        )
    }
}

// Keep asynchronous fixtures alive until the UI has inspected their state.
// Empty SSE heartbeats leave cancellation responsive and carry no chat data.
struct Stream {
    bytes: std::io::Cursor<Vec<u8>>,
    delay_ms: u64,
    gate: Option<(PathBuf, std::time::Instant)>,
}

fn gate_pending(path: &std::path::Path, remaining: bool) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
        Ok(metadata) if !metadata.is_file() || metadata.len() != 0 => Err(std::io::Error::other(
            "chat fixture gate must be an empty regular file",
        )),
        Ok(_) if !remaining => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "chat fixture gate expired",
        )),
        Ok(_) => Ok(true),
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some((path, deadline)) = &self.gate {
            if gate_pending(path, std::time::Instant::now() < *deadline)? {
                thread::sleep(Duration::from_millis(20));
                buffer[0] = b'\n';
                return Ok(1);
            }
            self.gate = None;
        }
        if self.delay_ms > 0 {
            thread::sleep(Duration::from_millis(self.delay_ms));
        }
        let n = buffer.len().min(80);
        self.bytes.read(&mut buffer[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_stream_yields_heartbeats_then_releases_unchanged_bytes() {
        let root = crate::test_support::workspace("stillus-chat-gate");
        let gate = root.join("gate");
        std::fs::write(&gate, []).unwrap();
        let mut stream = Stream {
            bytes: std::io::Cursor::new(b"response".to_vec()),
            delay_ms: 0,
            gate: Some((
                gate.clone(),
                std::time::Instant::now() + Duration::from_secs(30),
            )),
        };
        assert_eq!(stream.read(&mut []).unwrap(), 0);
        let mut bytes = [0; 16];
        assert_eq!(stream.read(&mut bytes).unwrap(), 1);
        assert_eq!(bytes[0], b'\n');
        assert_eq!(stream.bytes.position(), 0);
        std::fs::remove_file(&gate).unwrap();
        assert_eq!(stream.read(&mut bytes).unwrap(), 8);
        assert_eq!(&bytes[..8], b"response");
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gate_rejects_invalid_markers_and_has_a_fixed_deadline() {
        let root = crate::test_support::workspace("stillus-chat-gate-bound");
        let gate = root.join("gate");
        assert!(!gate_pending(&gate, false).unwrap());
        assert!(gate_pending(&root, true).is_err());
        std::fs::write(&gate, b"invalid").unwrap();
        assert!(gate_pending(&gate, true).is_err());
        std::fs::write(&gate, []).unwrap();
        assert!(gate_pending(&gate, true).unwrap());
        assert_eq!(
            gate_pending(&gate, false).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
