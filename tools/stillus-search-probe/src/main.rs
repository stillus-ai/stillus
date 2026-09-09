// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use stillus_search::SearchIndex;

const NOTE_COUNT: usize = 10_000;
const QUERY_RUNS: usize = 100;
const QUERY_P95_TARGET_MS: u128 = 100;

fn main() {
    if let Err(error) = run() {
        eprintln!("SEARCH_PROBE_ERROR {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or("missing command")?;
    match command.as_str() {
        "prepare" => {
            let workspace = PathBuf::from(args.next().ok_or("missing workspace")?);
            let large_file = PathBuf::from(args.next().ok_or("missing large file")?);
            prepare(&workspace, &large_file)?;
        }
        "probe" => {
            let workspace = PathBuf::from(args.next().ok_or("missing workspace")?);
            let label = args.next().ok_or("missing label")?;
            probe(&workspace, &label)?;
        }
        "query" => {
            let workspace = PathBuf::from(args.next().ok_or("missing workspace")?);
            let query = args.next().ok_or("missing query")?;
            let index = SearchIndex::open_or_rebuild(&workspace)?;
            println!("SEARCH_QUERY count={}", index.query(&query, 20)?.len());
        }
        _ => return Err(format!("unknown command {command}").into()),
    }
    Ok(())
}

fn prepare(workspace: &Path, large_file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let notes = workspace.join("notes");
    fs::create_dir_all(&notes)?;
    let marker = workspace.join(".prepared-v1");
    if marker.is_file() && notes.join("Large Body.md").is_file() {
        return Ok(());
    }
    for index in 0..NOTE_COUNT {
        let path = notes.join(format!("Corpus {index:05}.md"));
        if !path.exists() {
            fs::write(
                path,
                format!(
                    "---\ntitle: 'Corpus {index:05}'\ntags: ['Benchmark', 'Shard {}']\n---\nDeterministic local search note {index}. reproduciblemarker commonbodyterm\n",
                    index % 100
                ),
            )?;
        }
    }
    let destination = notes.join("Large Body.md");
    if destination.exists() {
        fs::remove_file(&destination)?;
    }
    if fs::hard_link(large_file, &destination).is_err() {
        fs::copy(large_file, &destination)?;
    }
    fs::write(marker, b"prepared\n")?;
    Ok(())
}

fn probe(workspace: &Path, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let open_started = Instant::now();
    let index = SearchIndex::open_or_rebuild(workspace)?;
    let open_ms = open_started.elapsed().as_millis();
    let queries = [
        "corpus 09999",
        "benchmark",
        "reproduciblemarker",
        "commonbodyterm",
    ];
    let mut samples = Vec::with_capacity(QUERY_RUNS);
    for run in 0..QUERY_RUNS {
        let started = Instant::now();
        let results = index.query(queries[run % queries.len()], 20)?;
        if results.is_empty() {
            return Err("benchmark query returned no results".into());
        }
        samples.push(started.elapsed().as_micros());
    }
    samples.sort_unstable();
    let p95_us = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    let rss_kib = rss_kib().unwrap_or(0);
    println!(
        "SEARCH_RESULT label={label} notes={NOTE_COUNT} open_ms={open_ms} query_p95_us={p95_us} rss_kib={rss_kib}"
    );
    if p95_us > QUERY_P95_TARGET_MS * 1_000 {
        return Err(format!("query p95 {p95_us}us exceeds {QUERY_P95_TARGET_MS}ms target").into());
    }
    Ok(())
}

fn rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}
