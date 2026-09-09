// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use stillus_engine::ItemId;
use stillus_rss::{RssRefreshRequest, RssRefreshResult, execute_refresh};

fn serve(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/feed", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        for response in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "HTTP test server timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("HTTP test server failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
                assert!(request.len() < 16_384);
            }
            requests.push(request);
            stream.write_all(response.as_bytes()).unwrap();
        }
        requests
    });
    (url, worker)
}

fn request(url: String) -> RssRefreshRequest {
    RssRefreshRequest {
        item_id: ItemId::new("feeds/test").unwrap(),
        url,
        etag: None,
        last_modified: None,
    }
}

#[test]
fn fetches_http_redirect_and_revalidates_cached_feed() {
    let body = r#"<rss version="2.0"><channel><title>Local feed</title><link>http://localhost/</link><description>Feed</description><item><guid>one</guid><title>Article</title><link>http://localhost/article</link></item></channel></rss>"#;
    let modified = "Mon, 01 Sep 2025 10:00:00 GMT";
    let (url, worker) = serve(vec![
        "HTTP/1.1 302 Found\r\nLocation: /actual\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nETag: \"one\"\r\nLast-Modified: {modified}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_owned(),
    ]);
    let RssRefreshResult::Fetched { cache, .. } = execute_refresh(request(url.clone())).unwrap()
    else {
        panic!("expected fetched feed");
    };
    assert_eq!(cache.title.as_deref(), Some("Local feed"));
    assert_eq!(
        cache.entries[0].link.as_deref(),
        Some("http://localhost/article")
    );
    assert_eq!(cache.etag.as_deref(), Some("\"one\""));
    assert_eq!(cache.last_modified.as_deref(), Some(modified));
    let mut conditional = request(url);
    conditional.etag = cache.etag;
    conditional.last_modified = cache.last_modified;
    assert!(matches!(
        execute_refresh(conditional).unwrap(),
        RssRefreshResult::NotModified { .. }
    ));
    let requests = worker.join().unwrap();
    assert!(requests[0].starts_with("GET /feed HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("GET /actual HTTP/1.1\r\n"));
    let headers = requests[2].to_ascii_lowercase();
    assert!(headers.contains("if-none-match: \"one\"\r\n"));
    assert!(headers.contains(&format!(
        "if-modified-since: {}\r\n",
        modified.to_ascii_lowercase()
    )));
}
