// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use stillus_core::{
    DocumentSession, EditorCommand, MAX_VIEWPORT_BYTES, MAX_VIEWPORT_LINES, ViewportRequest,
    ViewportSnapshot,
};

const INSERT: &str = "ввод 🦀 e\u{301}";
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let [path, run_label] = args.as_slice() else {
        return Err("usage: stillus-ui-probe FILE RUN".to_owned());
    };
    probe(Path::new(path), run_label)
}

fn probe(path: &Path, run_label: &str) -> Result<(), String> {
    let expected_size =
        usize::try_from(fs::metadata(path).map_err(display_error)?.len()).map_err(display_error)?;
    let rss_before = process_value("VmRSS:")?;
    let open_started = Instant::now();
    let mut document = DocumentSession::from_reader(
        0,
        "viewport probe",
        File::open(path).map_err(display_error)?,
    )
    .map_err(display_error)?;
    let open_elapsed = open_started.elapsed();
    if document.len_bytes() != expected_size {
        return Err(format!(
            "open length mismatch: expected {expected_size}, got {}",
            document.len_bytes()
        ));
    }
    let rss_open = process_value("VmRSS:")?;
    let total_lines = document.line_count();
    let positions = [0, total_lines / 2, total_lines.saturating_sub(48)];
    let mut viewport_times = Vec::with_capacity(positions.len());
    let mut checksum = FNV_OFFSET_BASIS;
    let mut max_snapshot_bytes = 0_usize;
    let mut max_snapshot_lines = 0_usize;
    for first_line in positions {
        let started = Instant::now();
        let snapshot = document
            .viewport(ViewportRequest {
                first_line,
                visible_lines: 48,
                overscan_lines: 8,
            })
            .map_err(display_error)?;
        viewport_times.push(started.elapsed());
        validate_snapshot(&snapshot)?;
        max_snapshot_bytes = max_snapshot_bytes.max(snapshot.rendered_bytes);
        max_snapshot_lines = max_snapshot_lines.max(snapshot.lines.len());
        checksum = checksum_snapshot(checksum, &snapshot);
    }

    let edit_started = Instant::now();
    document
        .apply(EditorCommand::Insert(INSERT.to_owned()))
        .map_err(display_error)?;
    document.apply(EditorCommand::Undo).map_err(display_error)?;
    let edit_elapsed = edit_started.elapsed();
    if document.len_bytes() != expected_size {
        return Err("insert/undo did not restore input length".to_owned());
    }
    let edited_snapshot = document
        .viewport(ViewportRequest::default())
        .map_err(display_error)?;
    validate_snapshot(&edited_snapshot)?;
    checksum = checksum_snapshot(checksum, &edited_snapshot);

    viewport_times.sort_unstable();
    let viewport_total: Duration = viewport_times.iter().copied().sum();
    let viewport_p95 = viewport_times[(viewport_times.len() - 1) * 95 / 100];
    let rss_after = process_value("VmRSS:")?;
    let peak_rss = process_value("VmHWM:")?;
    println!(
        "VIEWPORT_RESULT run={} bytes={} lines={} open_ms={:.3} viewport_total_us={} viewport_p95_us={} edit_us={} snapshot_max_bytes={} snapshot_max_lines={} checksum={checksum:016x} rss_before_kib={} rss_open_kib={} rss_after_kib={} peak_rss_kib={}",
        run_label,
        expected_size,
        total_lines,
        open_elapsed.as_secs_f64() * 1000.0,
        viewport_total.as_micros(),
        viewport_p95.as_micros(),
        edit_elapsed.as_micros(),
        max_snapshot_bytes,
        max_snapshot_lines,
        rss_before,
        rss_open,
        rss_after,
        peak_rss,
    );
    Ok(())
}

fn validate_snapshot(snapshot: &ViewportSnapshot) -> Result<(), String> {
    if snapshot.rendered_bytes > MAX_VIEWPORT_BYTES || snapshot.lines.len() > MAX_VIEWPORT_LINES {
        return Err("viewport exceeded hard bounds".to_owned());
    }
    if snapshot
        .lines
        .iter()
        .any(|line| line.start > line.end || !line.text.is_char_boundary(line.text.len()))
    {
        return Err("viewport returned an invalid range or UTF-8 boundary".to_owned());
    }
    Ok(())
}

fn checksum_snapshot(mut hash: u64, snapshot: &ViewportSnapshot) -> u64 {
    for line in &snapshot.lines {
        for byte in line
            .line_index
            .to_le_bytes()
            .into_iter()
            .chain(line.text.bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

fn process_value(label: &str) -> Result<u64, String> {
    let status = fs::read_to_string("/proc/self/status").map_err(display_error)?;
    status
        .lines()
        .find(|line| line.starts_with(label))
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .ok_or_else(|| format!("{label} not found in /proc/self/status"))?
        .parse::<u64>()
        .map_err(display_error)
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
