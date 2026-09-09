// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! The only network client in the update path.
//!
//! Requests go to a fixed set of GitHub hosts over HTTPS. Redirects are
//! followed manually so that every hop is checked against the same list, and
//! response bodies are bounded before they reach memory.

use crate::UpdateError;
use std::io::Read;
use std::time::Duration;
use url::Url;

const MAX_HOPS: usize = 4;
const CHUNK_BYTES: usize = 64 * 1024;
const USER_AGENT: &str = concat!("Stillus/", env!("CARGO_PKG_VERSION"));
const ALLOWED_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Fetches a bounded response body. Implementations must never return server
/// supplied text as part of an error.
pub trait UpdateTransport: Send + Sync {
    fn fetch(
        &self,
        url: &str,
        accept: &str,
        limit: u64,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, UpdateError>;
}

pub struct HttpsTransport;

impl UpdateTransport for HttpsTransport {
    fn fetch(
        &self,
        url: &str,
        accept: &str,
        limit: u64,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, UpdateError> {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .proxy(None)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(Duration::from_secs(600)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut target = allowed(url)?;
        for _ in 0..MAX_HOPS {
            let mut response = agent
                .get(target.as_str())
                .header("User-Agent", USER_AGENT)
                .header("Accept", accept)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .map_err(|_| UpdateError::Network)?;
            let status = response.status().as_u16();
            if let Some(location) = redirect(status, &response) {
                target = allowed(&join(&target, &location)?)?;
                continue;
            }
            match status {
                200 => {}
                403 | 429 => return Err(UpdateError::RateLimited),
                404 => return Err(UpdateError::Response),
                _ => return Err(UpdateError::Network),
            }
            let total = response
                .body()
                .content_length()
                .filter(|size| *size <= limit);
            let mut reader = response.body_mut().with_config().limit(limit).reader();
            return read(&mut reader, limit, total, progress);
        }
        Err(UpdateError::Network)
    }
}

fn read(
    reader: &mut impl Read,
    limit: u64,
    total: Option<u64>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, UpdateError> {
    let mut chunk = vec![0u8; CHUNK_BYTES];
    let mut body = Vec::new();
    loop {
        let count = reader.read(&mut chunk).map_err(|_| UpdateError::Network)?;
        if count == 0 {
            progress(body.len() as u64, total);
            return Ok(body);
        }
        if body.len() as u64 + count as u64 > limit {
            return Err(UpdateError::Response);
        }
        body.extend_from_slice(&chunk[..count]);
        progress(body.len() as u64, total);
    }
}

fn redirect(status: u16, response: &ureq::http::Response<ureq::Body>) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn join(base: &Url, location: &str) -> Result<String, UpdateError> {
    base.join(location)
        .map(String::from)
        .map_err(|_| UpdateError::Network)
}

/// Accepts only HTTPS URLs on the project's release hosts, without embedded
/// credentials or a non-default port.
fn allowed(url: &str) -> Result<Url, UpdateError> {
    let parsed = Url::parse(url).map_err(|_| UpdateError::Network)?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if parsed.scheme() != "https"
        || !ALLOWED_HOSTS.contains(&host.as_str())
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return Err(UpdateError::Network);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_release_hosts_are_reachable() {
        assert!(allowed("https://api.github.com/repos/stillus-ai/stillus/releases/latest").is_ok());
        assert!(allowed("https://release-assets.githubusercontent.com/x").is_ok());
        for rejected in [
            "http://api.github.com/x",
            "https://example.com/x",
            "https://api.github.com.example.com/x",
            "https://user:secret@api.github.com/x",
            "https://api.github.com:8443/x",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert_eq!(allowed(rejected), Err(UpdateError::Network), "{rejected}");
        }
    }

    #[test]
    fn redirects_resolve_against_the_previous_hop() {
        let base =
            allowed("https://github.com/stillus-ai/stillus/releases/download/v1.0.0/a").unwrap();
        assert_eq!(
            join(&base, "/stillus-ai/stillus/b").unwrap(),
            "https://github.com/stillus-ai/stillus/b"
        );
        let hop = join(&base, "https://example.com/evil").unwrap();
        assert_eq!(allowed(&hop), Err(UpdateError::Network));
    }

    #[test]
    fn bodies_stop_at_the_limit_and_report_progress() {
        let data = vec![7u8; CHUNK_BYTES * 2 + 5];
        let mut seen = Vec::new();
        let body = read(
            &mut data.as_slice(),
            data.len() as u64,
            Some(9),
            &mut |read, total| {
                seen.push((read, total));
            },
        )
        .unwrap();
        assert_eq!(body, data);
        assert_eq!(seen.last(), Some(&(data.len() as u64, Some(9))));
        assert_eq!(
            read(&mut data.as_slice(), 10, None, &mut |_, _| {}),
            Err(UpdateError::Response)
        );
    }
}
