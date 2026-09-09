// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::journal::{RequestJournal, RequestRecord, RequestStatus, now_ms, safe_response};
use crate::{AiError, AiModel, AiProvider, ApiKey, models};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const MAX_PAGE_BYTES: u64 = 512 * 1024;
const MAX_MODELS: usize = 2000;

pub trait CatalogTransport: Send + Sync {
    fn list(&self, provider: AiProvider, key: &ApiKey) -> Result<Vec<AiModel>, AiError>;

    fn list_recorded(
        &self,
        provider: AiProvider,
        key: &ApiKey,
        journal: &dyn RequestJournal,
        operation: &str,
    ) -> Result<Vec<AiModel>, AiError> {
        let mut record = journal.begin(RequestRecord::catalog(provider, operation, None))?;
        let result = self.list(provider, key);
        record.duration_ms = Some(now_ms().saturating_sub(record.started_ms));
        match &result {
            Ok(models) => {
                record.status = RequestStatus::Success;
                record.http_status = Some(200);
                record.response = Some(safe_response(
                    &serde_json::to_vec(models).map_err(|_| AiError::Response)?,
                    key,
                ));
            }
            Err(error) => {
                record.status = RequestStatus::Error;
                record.error = Some(format!("{error:?}"));
            }
        }
        journal.complete(record);
        result
    }
}

pub struct HttpsCatalogTransport;

impl CatalogTransport for HttpsCatalogTransport {
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
        if crate::detect_provider(key.expose()) != Some(provider) {
            return Err(AiError::KeyFormat);
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .https_only(true)
            .max_redirects(0)
            .proxy(None)
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let start = Instant::now();
        collect(provider, |cursor| {
            if start.elapsed() > Duration::from_secs(30) {
                return Err(AiError::Network);
            }
            let mut request = match provider {
                AiProvider::OpenAi => agent
                    .get("https://api.openai.com/v1/models")
                    .header("Authorization", format!("Bearer {}", key.expose())),
                AiProvider::Anthropic => {
                    let request = agent
                        .get("https://api.anthropic.com/v1/models")
                        .header("x-api-key", key.expose())
                        .header("anthropic-version", "2023-06-01")
                        .query("limit", "1000");
                    if let Some(cursor) = cursor {
                        request.query("after_id", cursor)
                    } else {
                        request
                    }
                }
            };
            request = request.header("Accept", "application/json");
            recorded_page(
                journal,
                RequestRecord::catalog(provider, operation, cursor),
                key,
                |record| {
                    let mut response = request.call().map_err(|_| AiError::Network)?;
                    record.http_status = Some(response.status().as_u16());
                    response
                        .body_mut()
                        .with_config()
                        .limit(MAX_PAGE_BYTES)
                        .read_to_vec()
                        .map_err(|_| AiError::Response)
                },
            )
        })
    }
}

fn recorded_page(
    journal: &dyn RequestJournal,
    mut record: RequestRecord,
    key: &ApiKey,
    send: impl FnOnce(&mut RequestRecord) -> Result<Vec<u8>, AiError>,
) -> Result<Vec<u8>, AiError> {
    record.parameters = safe_response(
        &serde_json::to_vec(&record.parameters).map_err(|_| AiError::Response)?,
        key,
    );
    let mut record = journal.begin(record)?;
    let result = send(&mut record).and_then(|bytes| {
        if bytes.len() as u64 > MAX_PAGE_BYTES {
            return Err(AiError::Response);
        }
        record.response = Some(safe_response(&bytes, key));
        if let Some(status) = record.http_status.filter(|status| *status != 200) {
            return Err(status_error(status));
        }
        serde_json::from_slice::<Page>(&bytes).map_err(|_| AiError::Response)?;
        Ok(bytes)
    });
    record.duration_ms = Some(now_ms().saturating_sub(record.started_ms));
    record.status = if result.is_ok() {
        RequestStatus::Success
    } else {
        RequestStatus::Error
    };
    record.error = result.as_ref().err().map(|error| format!("{error:?}"));
    journal.complete(record);
    result
}

fn status_error(code: u16) -> AiError {
    match code {
        401 => AiError::Unauthorized,
        403 => AiError::Forbidden,
        429 => AiError::RateLimited,
        _ => AiError::Network,
    }
}

#[derive(Deserialize)]
struct Page {
    data: Vec<serde_json::Value>,
    #[serde(default)]
    has_more: bool,
    last_id: Option<String>,
}

fn collect(
    provider: AiProvider,
    mut fetch: impl FnMut(Option<&str>) -> Result<Vec<u8>, AiError>,
) -> Result<Vec<AiModel>, AiError> {
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    let mut count = 0usize;
    for _ in 0..10 {
        let bytes = fetch(cursor.as_deref())?;
        if bytes.len() as u64 > MAX_PAGE_BYTES {
            return Err(AiError::Response);
        }
        let page: Page = serde_json::from_slice(&bytes).map_err(|_| AiError::Response)?;
        count += page.data.len();
        if count > MAX_MODELS {
            return Err(AiError::Response);
        }
        for raw in page.data {
            if let Some(model) = models::model(provider, &raw)
                && ids.insert(model.id.clone())
            {
                result.push(model);
            }
        }
        if !page.has_more {
            result.sort_by(|a, b| a.name.cmp(&b.name));
            return if result.is_empty() {
                Err(AiError::NoModels)
            } else {
                Ok(result)
            };
        }
        if provider != AiProvider::Anthropic {
            return Err(AiError::Response);
        }
        let next = page
            .last_id
            .filter(|id| models::valid_id(id))
            .ok_or(AiError::Response)?;
        if !cursors.insert(next.clone()) {
            return Err(AiError::Response);
        }
        cursor = Some(next);
    }
    Err(AiError::Response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Journal {
        records: std::sync::Mutex<Vec<RequestRecord>>,
        blocked: bool,
    }
    impl RequestJournal for Journal {
        fn begin(&self, record: RequestRecord) -> Result<RequestRecord, AiError> {
            if self.blocked {
                return Err(AiError::Journal);
            }
            self.records.lock().unwrap().push(record.clone());
            Ok(record)
        }
        fn complete(&self, record: RequestRecord) {
            *self.records.lock().unwrap().last_mut().unwrap() = record;
        }
    }
    #[test]
    fn every_catalog_page_is_recorded_and_credential_cursors_are_redacted() {
        let journal = Journal::default();
        let (_, key) = ApiKey::parse(zeroize::Zeroizing::new(
            "sk-ant-api-abcdefghijklmnopqrstuv".into(),
        ))
        .unwrap();
        let models=collect(AiProvider::Anthropic,|cursor|recorded_page(&journal,RequestRecord::catalog(AiProvider::Anthropic,"catalog/test",cursor),&key,|record|{
            record.http_status=Some(200);
            Ok(if cursor.is_none(){serde_json::to_vec(&serde_json::json!({"data":[{"id":"claude-opus-4-6"}],"has_more":true,"last_id":key.expose()})).unwrap()}
            else{br#"{"data":[{"id":"claude-sonnet-4-6"}]}"#.to_vec()})
        })).unwrap();
        assert_eq!(models.len(), 2);
        let records = journal.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.operation == "catalog/test"
            && r.status == RequestStatus::Success
            && r.http_status == Some(200)));
        assert!(
            !serde_json::to_string(&*records)
                .unwrap()
                .contains(key.expose())
        );
    }
    #[test]
    fn journal_precedes_network_and_keeps_http_and_parse_errors() {
        let (_, key) = ApiKey::parse(zeroize::Zeroizing::new(
            "sk-proj-abcdefghijklmnopqrstuv".into(),
        ))
        .unwrap();
        let blocked = Journal {
            blocked: true,
            ..Journal::default()
        };
        assert_eq!(
            recorded_page(
                &blocked,
                RequestRecord::catalog(AiProvider::OpenAi, "test", None),
                &key,
                |_| panic!("network must not run")
            ),
            Err(AiError::Journal)
        );
        let journal = Journal::default();
        assert_eq!(
            recorded_page(
                &journal,
                RequestRecord::catalog(AiProvider::OpenAi, "test", None),
                &key,
                |r| {
                    r.http_status = Some(401);
                    Ok(b"denied".to_vec())
                }
            ),
            Err(AiError::Unauthorized)
        );
        assert_eq!(
            journal.records.lock().unwrap()[0].status,
            RequestStatus::Error
        );
        assert_eq!(
            recorded_page(
                &journal,
                RequestRecord::catalog(AiProvider::OpenAi, "test", None),
                &key,
                |r| {
                    r.http_status = Some(200);
                    Ok(b"invalid json".to_vec())
                }
            ),
            Err(AiError::Response)
        );
        assert_eq!(journal.records.lock().unwrap()[1].http_status, Some(200));
    }

    #[test]
    fn bounded_pagination_and_errors_never_include_response_text() {
        let mut calls = 0;
        let models = collect(AiProvider::Anthropic, |cursor| {
            calls += 1;
            if cursor.is_none() { Ok(br#"{"data":[{"id":"claude-opus-4-6"}],"has_more":true,"last_id":"claude-opus-4-6"}"#.to_vec()) }
            else { Ok(br#"{"data":[{"id":"claude-sonnet-4-6"}]}"#.to_vec()) }
        }).unwrap();
        assert_eq!(calls, 2);
        assert_eq!(models.len(), 2);
        assert_eq!(
            collect(AiProvider::Anthropic, |_| Ok(
                br#"{"data":[],"has_more":true,"last_id":"same"}"#.to_vec()
            )),
            Err(AiError::Response)
        );
        assert_eq!(
            collect(AiProvider::OpenAi, |_| Ok(b"secret-token".to_vec())),
            Err(AiError::Response)
        );
        assert_eq!(status_error(401), AiError::Unauthorized);
        assert_eq!(status_error(403), AiError::Forbidden);
        assert_eq!(status_error(429), AiError::RateLimited);
    }
}
