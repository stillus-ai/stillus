// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
#![cfg(test)]

//! Proves that the update client cannot be pointed at another host. A loopback
//! listener stands in for the host an attacker would want reached; the test
//! passes only when nothing ever connects to it.

use std::io::ErrorKind;
use std::net::TcpListener;

use stillus_update::{HttpsTransport, UpdateError, UpdateTransport};

#[test]
fn the_update_client_only_talks_to_github_over_https() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let transport = HttpsTransport;
    for url in [
        format!("http://127.0.0.1:{port}/repos/stillus-ai/stillus/releases/latest"),
        format!("https://127.0.0.1:{port}/repos/stillus-ai/stillus/releases/latest"),
        format!("https://api.github.com:{port}/repos/stillus-ai/stillus/releases/latest"),
        format!("https://api.github.com.localhost:{port}/releases/latest"),
        format!("https://user:secret@api.github.com:{port}/releases/latest"),
    ] {
        let result = transport.fetch(&url, "application/json", 1024, &mut |_, _| {});
        assert_eq!(result, Err(UpdateError::Network), "{url}");
    }
    let waiting = listener.accept().map(|_| ()).map_err(|error| error.kind());
    assert_eq!(waiting, Err(ErrorKind::WouldBlock));
}
