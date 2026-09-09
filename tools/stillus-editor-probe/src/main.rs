// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use stillus_editor::{ByteOffset, ByteRange, Editor};

const INSERT: &str = "<insert>\nвставка 🦀 e\u{301}\n";
const REPLACEMENT: &str = "<replace>\nзамена 🌿\n";
const BRANCH: &str = "<branch>\nновая ветка\n";
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
        return Err("usage: stillus-editor-probe FILE RUN".to_owned());
    };
    probe(Path::new(path), run_label)
}

fn probe(path: &Path, run_label: &str) -> Result<(), String> {
    let expected_size =
        usize::try_from(fs::metadata(path).map_err(display_error)?.len()).map_err(display_error)?;
    let rss_before = process_value("VmRSS:")?;
    let open_started = Instant::now();
    let mut editor =
        Editor::from_reader(File::open(path).map_err(display_error)?).map_err(display_error)?;
    let open_elapsed = open_started.elapsed();
    if editor.len_bytes() != expected_size {
        return Err(format!(
            "open length mismatch: expected {expected_size}, got {}",
            editor.len_bytes()
        ));
    }
    let rss_open = process_value("VmRSS:")?;
    let initial_checksum = editor.checksum_fnv1a();
    let initial_len = editor.len_bytes();
    let positions = [
        ByteOffset::new(0),
        floor_boundary(&editor, initial_len / 2)?,
        ByteOffset::new(initial_len),
    ];
    let line_probe = positions
        .iter()
        .map(|offset| editor.line_of_offset(*offset).map_err(display_error))
        .collect::<Result<Vec<_>, _>>()?;

    let mut operation_times = Vec::with_capacity(10);
    for position in positions {
        timed(&mut operation_times, || editor.insert(position, INSERT))?;
        timed(&mut operation_times, || {
            editor.replace(
                ByteRange::new(position, ByteOffset::new(position.get() + INSERT.len()))?,
                REPLACEMENT,
            )
        })?;
        timed(&mut operation_times, || {
            editor.delete(ByteRange::new(
                position,
                ByteOffset::new(position.get() + REPLACEMENT.len()),
            )?)
        })?;
    }
    if editor.len_bytes() != initial_len {
        return Err("edit cycle did not restore input length".to_owned());
    }
    for _ in 0..9 {
        if !editor.undo() {
            return Err("undo history ended early".to_owned());
        }
    }
    if editor.len_bytes() != initial_len {
        return Err("undo did not restore input length".to_owned());
    }
    for _ in 0..9 {
        if !editor.redo() {
            return Err("redo history ended early".to_owned());
        }
    }
    if editor.len_bytes() != initial_len || !editor.undo() {
        return Err("redo/branch setup failed".to_owned());
    }
    let branch_at = ByteOffset::new(editor.len_bytes());
    timed(&mut operation_times, || editor.insert(branch_at, BRANCH))?;
    if editor.redo() {
        return Err("new edit branch did not clear redo".to_owned());
    }

    let final_checksum = editor.checksum_fnv1a();
    let expected_checksum = fnv_update(
        fnv_update(initial_checksum, REPLACEMENT.as_bytes()),
        BRANCH.as_bytes(),
    );
    if editor.len_bytes() != initial_len + REPLACEMENT.len() + BRANCH.len()
        || final_checksum != expected_checksum
    {
        return Err("final size/checksum differs from streaming reference".to_owned());
    }

    let rss_edits = process_value("VmRSS:")?;
    let snapshot_started = Instant::now();
    let snapshot = editor.snapshot();
    let snapshot_clone_elapsed = snapshot_started.elapsed();
    let stream_started = Instant::now();
    snapshot.write_to(io::sink()).map_err(display_error)?;
    let snapshot_stream_elapsed = stream_started.elapsed();
    if snapshot.len_bytes() != editor.len_bytes()
        || snapshot.checksum_fnv1a() != editor.checksum_fnv1a()
    {
        return Err("COW save snapshot differs from editor".to_owned());
    }

    operation_times.sort_unstable();
    let edit_total: Duration = operation_times.iter().copied().sum();
    let edit_p50 = percentile(&operation_times, 50);
    let edit_p95 = percentile(&operation_times, 95);
    let rss_snapshot = process_value("VmRSS:")?;
    let peak_rss = process_value("VmHWM:")?;
    let threads = process_value("Threads:")?;
    println!(
        "EDITOR_RESULT run={} bytes={} initial_checksum={initial_checksum:016x} final_checksum={final_checksum:016x} open_ms={:.3} edit_total_us={} edit_p50_us={} edit_p95_us={} snapshot_clone_us={} snapshot_stream_us={} rss_before_kib={} rss_open_kib={} rss_edits_kib={} rss_snapshot_kib={} peak_rss_kib={} history_bytes={} threads={} lines={:?}",
        run_label,
        expected_size,
        open_elapsed.as_secs_f64() * 1000.0,
        edit_total.as_micros(),
        edit_p50.as_micros(),
        edit_p95.as_micros(),
        snapshot_clone_elapsed.as_micros(),
        snapshot_stream_elapsed.as_micros(),
        rss_before,
        rss_open,
        rss_edits,
        rss_snapshot,
        peak_rss,
        editor.history_bytes(),
        threads,
        line_probe,
    );
    Ok(())
}

fn floor_boundary(editor: &Editor, mut offset: usize) -> Result<ByteOffset, String> {
    while !editor
        .is_codepoint_boundary(ByteOffset::new(offset))
        .map_err(display_error)?
    {
        offset -= 1;
    }
    Ok(ByteOffset::new(offset))
}

fn timed<T>(
    timings: &mut Vec<Duration>,
    operation: impl FnOnce() -> Result<T, stillus_editor::EditorError>,
) -> Result<(), String> {
    let started = Instant::now();
    operation().map_err(display_error)?;
    timings.push(started.elapsed());
    Ok(())
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    values[(values.len() - 1) * percentile / 100]
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

fn fnv_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
