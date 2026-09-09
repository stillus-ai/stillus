// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Default)]
struct Journal {
    records: Mutex<Vec<RequestRecord>>,
    blocked: bool,
}
impl RequestJournal for Journal {
    fn begin(&self, mut r: RequestRecord) -> Result<RequestRecord, AiError> {
        if self.blocked {
            return Err(AiError::Journal);
        }
        r.id = "request/1".into();
        self.records.lock().unwrap().push(r.clone());
        Ok(r)
    }
    fn complete(&self, r: RequestRecord) {
        *self.records.lock().unwrap().last_mut().unwrap() = r;
    }
}
struct Transport {
    bytes: Vec<u8>,
    calls: AtomicUsize,
    split: usize,
}
impl ResponsesTransport for Transport {
    fn send(
        &self,
        _: &ApiKey,
        body: &[u8],
        consume: &mut dyn FnMut(u16, &mut dyn Read) -> Result<(), AiError>,
    ) -> Result<(), AiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["stream"], true);
        struct Split<'a> {
            bytes: &'a [u8],
            split: usize,
        }
        impl Read for Split<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.bytes.len().min(buf.len()).min(self.split);
                buf[..n].copy_from_slice(&self.bytes[..n]);
                self.bytes = &self.bytes[n..];
                Ok(n)
            }
        }
        consume(
            200,
            &mut Split {
                bytes: &self.bytes,
                split: self.split,
            },
        )
    }
}
fn key() -> ApiKey {
    ApiKey::parse(zeroize::Zeroizing::new(
        "sk-proj-abcdefghijklmnopqrstuvwxyz".into(),
    ))
    .unwrap()
    .1
}
fn request() -> Generation {
    Generation {
        content_policy: journal::ContentPolicy::Public,
        model: "gpt-5.6-luna".into(),
        effort: Some(crate::AiEffort::High),
        input: vec![InputItem::User("hello".into())],
        tools: vec![ToolDefinition {
            name: "notes/create".into(),
            description: "Create a note".into(),
            schema: json!({"type":"object","properties":{"text":{"type":"string"},"title":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }],
        summary: false,
        context_summary: None,
        chat: "chat/1".into(),
        run: "run/1".into(),
        step: 1,
        max_output: 8192,
    }
}
fn sse(events: Vec<Value>) -> Vec<u8> {
    events
        .into_iter()
        .map(|e| format!("data: {e}\r\n\r\n"))
        .collect::<String>()
        .into_bytes()
}
#[test]
fn fragmented_utf8_stream_strict_calls_usage_and_local_continuation() {
    let req = request();
    let names = ToolNames::new(&req.tools).unwrap();
    let output = json!([{"type":"reasoning","encrypted_content":"opaque"},{"type":"function_call","id":"item/1","status":"completed","name":names.wire("notes/create").unwrap(),"call_id":"call/1","arguments":"{\"text\":\"Привет\",\"title\":null}"}]);
    let bytes = sse(vec![
        json!({"type":"response.output_text.delta","delta":"Привет"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"item/1","delta":"{\"text\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"item/1","delta":"\"Привет\",\"title\":null}"}),
        json!({"type":"response.function_call_arguments.done","item_id":"item/1","arguments":"{\"text\":\"Привет\",\"title\":null}"}),
        json!({"type":"response.completed","response":{"output":output,"usage":{"input_tokens":12,"output_tokens":4}}}),
    ]);
    for split in 1..17 {
        let transport = Arc::new(Transport {
            bytes: bytes.clone(),
            calls: AtomicUsize::new(0),
            split,
        });
        let journal = Journal::default();
        let mut events = Vec::new();
        let result = OpenAiEngine::with_transport(transport)
            .generate(&req, &key(), &journal, &Cancellation::default(), &mut |e| {
                events.push(e)
            })
            .unwrap();
        assert_eq!(result.text, "Привет");
        assert_eq!(result.calls.len(), 1);
        assert_eq!(result.calls[0].arguments, json!({"text":"Привет"}));
        assert_eq!(result.usage.input, Some(12));
        assert_eq!(journal.records.lock().unwrap()[0].id, "request/1");
        let mut follow = req.clone();
        follow.input.push(InputItem::Assistant {
            text: result.text,
            state: result.state,
        });
        let body = request_body(&follow, &names).unwrap();
        assert_eq!(body["input"][1]["encrypted_content"], "opaque");
    }
}
#[test]
fn journal_blocks_before_http_and_broken_stream_never_emits_a_tool() {
    let transport = Arc::new(Transport {
        bytes: sse(vec![
            json!({"type":"response.output_text.delta","delta":"partial"}),
            json!({"type":"response.function_call_arguments.done","item_id":"item/1","arguments":"{}"}),
        ]),
        calls: AtomicUsize::new(0),
        split: 1,
    });
    let engine = OpenAiEngine::with_transport(transport.clone());
    assert!(matches!(
        engine.generate(
            &request(),
            &key(),
            &Journal {
                blocked: true,
                ..Default::default()
            },
            &Cancellation::default(),
            &mut |_| panic!("no events")
        ),
        Err(AiError::Journal)
    ));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    let mut text = String::new();
    let journal = Journal::default();
    assert!(matches!(
        engine.generate(
            &request(),
            &key(),
            &journal,
            &Cancellation::default(),
            &mut |e| match e {
                GenerationEvent::Text(t) => text = t,
                GenerationEvent::Tool(_) => panic!("partial call"),
                _ => {}
            }
        ),
        Err(AiError::Response)
    ));
    assert_eq!(text, "partial");
    assert_eq!(
        journal.records.lock().unwrap()[0].status,
        RequestStatus::Error
    );
}
#[test]
fn split_secrets_never_reach_events_or_journal() {
    let key = key();
    let secret = key.expose();
    let bytes = sse(vec![
        json!({"type":"response.output_text.delta","delta":&secret[..15]}),
        json!({"type":"response.output_text.delta","delta":&secret[15..]}),
        json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":secret}]}],"usage":{}}}),
    ]);
    let journal = Journal::default();
    let result = OpenAiEngine::with_transport(Arc::new(Transport {
        bytes,
        calls: AtomicUsize::new(0),
        split: 1,
    }))
    .generate(
        &request(),
        &key,
        &journal,
        &Cancellation::default(),
        &mut |e| {
            if let GenerationEvent::Text(text) = e {
                assert!(!text.contains(secret));
                assert!(!text.contains("sk-proj"));
            }
        },
    )
    .unwrap();
    assert_eq!(result.text, "[redacted]");
    assert!(
        !serde_json::to_string(&*journal.records.lock().unwrap())
            .unwrap()
            .contains(secret)
    );
}
#[test]
fn strict_nested_schema_name_bijection_and_local_validation() {
    let schema = json!({"type":"object","properties":{"filter":{"type":"object","properties":{"value":{"type":"string"}},"required":[]}},"required":[]});
    let strict = strict_schema(&schema).unwrap();
    assert_eq!(
        strict_schema(&json!({"enum":["name","created"]})).unwrap()["type"],
        "string"
    );
    assert_eq!(strict["required"], json!(["filter"]));
    assert_eq!(
        strict["properties"]["filter"]["anyOf"][0]["additionalProperties"],
        false
    );
    assert!(native_arguments(&schema, json!({"unknown":1})).is_err());
    assert!(native_arguments(&schema, json!({"filter":{"value":3}})).is_err());
    let mut tools = request().tools;
    tools.push(ToolDefinition {
        name: "notes_create".into(),
        description: String::new(),
        schema: schema.clone(),
    });
    let names = ToolNames::new(&tools).unwrap();
    assert_ne!(
        names.wire("notes/create").unwrap(),
        names.wire("notes_create").unwrap()
    );
    assert_eq!(
        names.native(names.wire("notes/create").unwrap()).unwrap(),
        "notes/create"
    );
    tools.push(tools[0].clone());
    assert!(ToolNames::new(&tools).is_err());
}
#[test]
fn compression_retains_last_exchange_and_current_chain() {
    let input = vec![
        InputItem::User("one".into()),
        InputItem::Assistant {
            text: "answer".into(),
            state: None,
        },
        InputItem::User("two".into()),
        InputItem::Assistant {
            text: "answer".into(),
            state: None,
        },
        InputItem::User("three".into()),
        InputItem::ToolResult {
            call_id: "c".into(),
            name: "notes/read".into(),
            output: json!({}),
        },
    ];
    assert_eq!(compression_split(&input), Some(2));
}
#[test]
fn another_provider_only_needs_adapter_and_registration() {
    struct Second;
    impl AiProviderEngine for Second {
        fn id(&self) -> AiProvider {
            AiProvider::Other("fixture/provider".into())
        }
        fn name(&self) -> &str {
            "Fixture"
        }
        fn recognizes_key(&self, key: &str) -> bool {
            key == "fixture"
        }
        fn default_model(&self) -> &str {
            "fixture/model"
        }
        fn catalog(
            &self,
            _: &ApiKey,
            _: &dyn RequestJournal,
            _: &str,
        ) -> Result<Vec<crate::AiModel>, AiError> {
            Ok(vec![])
        }
        fn capabilities(&self, _: &str) -> Result<ModelCapabilities, AiError> {
            Ok(ModelCapabilities {
                generation: true,
                tools: true,
                context_tokens: 1000,
                output_tokens: 100,
            })
        }
        fn generate(
            &self,
            _: &Generation,
            _: &ApiKey,
            _: &dyn RequestJournal,
            _: &Cancellation,
            emit: &mut dyn FnMut(GenerationEvent),
        ) -> Result<GenerationResult, AiError> {
            let wire = json!({"chunks":[{"words":"second format"}]});
            let result = GenerationResult {
                text: wire["chunks"][0]["words"].as_str().unwrap().into(),
                ..Default::default()
            };
            emit(GenerationEvent::Text(result.text.clone()));
            Ok(result)
        }
    }
    let mut registry = ProviderRegistry::standard();
    registry.register(Arc::new(Second)).unwrap();
    let id = registry.detect("fixture").unwrap();
    assert_eq!(
        serde_json::from_str::<AiProvider>(&serde_json::to_string(&id).unwrap()).unwrap(),
        id
    );
    let adapter = registry.get(&id).unwrap();
    let credential =
        ApiKey::for_engine(zeroize::Zeroizing::new("fixture".into()), adapter.as_ref()).unwrap();
    assert!(matches!(
        ApiKey::for_engine(zeroize::Zeroizing::new("wrong".into()), adapter.as_ref()),
        Err(AiError::KeyFormat)
    ));
    assert_eq!(
        adapter
            .generate(
                &request(),
                &credential,
                &Journal::default(),
                &Cancellation::default(),
                &mut |_| {}
            )
            .unwrap()
            .text,
        "second format"
    );
}

#[test]
fn cancellation_after_durable_begin_prevents_http() {
    struct Cancelling(Cancellation, Mutex<Option<RequestRecord>>);
    impl RequestJournal for Cancelling {
        fn begin(&self, r: RequestRecord) -> Result<RequestRecord, AiError> {
            self.0.cancel();
            *self.1.lock().unwrap() = Some(r.clone());
            Ok(r)
        }
        fn complete(&self, r: RequestRecord) {
            *self.1.lock().unwrap() = Some(r);
        }
    }
    let transport = Arc::new(Transport {
        bytes: vec![],
        calls: AtomicUsize::new(0),
        split: 1,
    });
    let journal = Cancelling(Cancellation::default(), Mutex::new(None));
    assert!(matches!(
        OpenAiEngine::with_transport(transport.clone()).generate(
            &request(),
            &key(),
            &journal,
            &journal.0,
            &mut |_| {}
        ),
        Err(AiError::Cancelled)
    ));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal.1.lock().unwrap().as_ref().unwrap().status,
        RequestStatus::Error
    );
}
#[test]
fn inconsistent_argument_fragments_and_incomplete_calls_are_never_executed() {
    let request = request();
    let names = ToolNames::new(&request.tools).unwrap();
    for status in ["in_progress", "completed"] {
        let bytes = sse(vec![
            json!({"type":"response.function_call_arguments.delta","item_id":"i","delta":"{\"text\":\"one\"}"}),
            json!({"type":"response.completed","response":{"output":[{"type":"function_call","id":"i","call_id":"c","status":status,"name":names.wire("notes/create").unwrap(),"arguments":"{\"text\":\"two\"}"}]}}),
        ]);
        let engine = OpenAiEngine::with_transport(Arc::new(Transport {
            bytes,
            calls: AtomicUsize::new(0),
            split: 2,
        }));
        assert!(
            engine
                .generate(
                    &request,
                    &key(),
                    &Journal::default(),
                    &Cancellation::default(),
                    &mut |event| assert!(!matches!(event, GenerationEvent::Tool(_)))
                )
                .is_err()
        );
    }
}
#[test]
fn response_limit_and_http_errors_keep_safe_diagnostics() {
    let bytes = sse(vec![
        json!({"type":"response.output_text.delta","delta":"x".repeat(journal::MAX_RESPONSE_BYTES+1)}),
    ]);
    assert!(
        OpenAiEngine::with_transport(Arc::new(Transport {
            bytes,
            calls: AtomicUsize::new(0),
            split: 4096
        }))
        .generate(
            &request(),
            &key(),
            &Journal::default(),
            &Cancellation::default(),
            &mut |_| {}
        )
        .is_err()
    );
    struct Http(u16);
    impl ResponsesTransport for Http {
        fn send(
            &self,
            key: &ApiKey,
            _: &[u8],
            consume: &mut dyn FnMut(u16, &mut dyn Read) -> Result<(), AiError>,
        ) -> Result<(), AiError> {
            consume(
                self.0,
                &mut std::io::Cursor::new(format!("error {}", key.expose()).into_bytes()),
            )
        }
    }
    for status in [401, 403, 429, 500] {
        let journal = Journal::default();
        assert!(
            OpenAiEngine::with_transport(Arc::new(Http(status)))
                .generate(
                    &request(),
                    &key(),
                    &journal,
                    &Cancellation::default(),
                    &mut |_| {}
                )
                .is_err()
        );
        let records = journal.records.lock().unwrap();
        assert_eq!(records[0].http_status, Some(status));
        assert!(
            !serde_json::to_string(&*records)
                .unwrap()
                .contains(key().expose())
        );
    }
}

#[test]
fn protected_content_policy_omits_bodies_before_journal_callbacks() {
    let mut request = request();
    request.content_policy = journal::ContentPolicy::Protected;
    let journal = Journal::default();
    let bytes = sse(vec![
        json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"context"}]}]}}),
    ]);
    let result = OpenAiEngine::with_transport(Arc::new(Transport {
        bytes,
        calls: AtomicUsize::new(0),
        split: 1,
    }))
    .generate(
        &request,
        &key(),
        &journal,
        &Cancellation::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.text, "context");
    let records = journal.records.lock().unwrap();
    assert!(records[0].request.is_none());
    assert!(records[0].response.is_none());
    assert!(records[0].parameters.is_null());
}
