// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use lapce_xi_rope::Rope as LapceRope;
use lapce_xi_rope::rope::RopeInfo;
use lapce_xi_rope::tree::TreeBuilder;
use ropey::Rope as RopeyRope;

const DATASET_SIZES: [u64; 3] = [10_000_000, 100_000_000, 1_000_000_000];
const PATTERN: &str = "# Stillus deterministic probe\nASCII text; Кириллица; emoji 🦀; e\u{301}; 日本語.\n- short\n- a deliberately longer Markdown line used to exercise chunk and line lookup without depending on user data.\n\n";
const INSERT: &str = "<insert>\nвставка 🦀 e\u{301}\n";
const REPLACEMENT: &str = "<replace>\nзамена 🌿\n";
const BRANCH: &str = "<branch>\nновая ветка\n";
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command, directory] if command == "generate" => generate_all(Path::new(directory)),
        [command, candidate, path, run_label] if command == "probe" => {
            probe(Candidate::parse(candidate)?, Path::new(path), run_label)
        }
        _ => Err(
            "usage: stillus-buffer-probe generate DIR | probe (ropey|lapce) FILE RUN".to_owned(),
        ),
    }
}

fn generate_all(directory: &Path) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(display_error)?;
    for size in DATASET_SIZES {
        let path = directory.join(format!("stillus-{size}.md"));
        let checksum = generate_file(&path, size).map_err(display_error)?;
        println!(
            "GENERATED path={} bytes={} checksum={checksum:016x}",
            path.display(),
            fs::metadata(&path).map_err(display_error)?.len()
        );
    }
    Ok(())
}

fn generate_file(path: &Path, size: u64) -> io::Result<u64> {
    let mut output = BufWriter::with_capacity(1024 * 1024, File::create(path)?);
    let pattern = PATTERN.as_bytes();
    let mut remaining = size;
    let mut checksum = FNV_OFFSET_BASIS;
    while remaining >= pattern.len() as u64 {
        output.write_all(pattern)?;
        checksum = fnv_update(checksum, pattern);
        remaining -= pattern.len() as u64;
    }
    if remaining > 0 {
        let tail = vec![b'x'; remaining as usize];
        output.write_all(&tail)?;
        checksum = fnv_update(checksum, &tail);
    }
    output.flush()?;
    Ok(checksum)
}

#[derive(Clone, Copy, Debug)]
enum Candidate {
    Ropey,
    Lapce,
}

impl Candidate {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ropey" => Ok(Self::Ropey),
            "lapce" => Ok(Self::Lapce),
            _ => Err(format!("unknown candidate: {value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ropey => "ropey",
            Self::Lapce => "lapce-xi-rope",
        }
    }
}

#[derive(Clone)]
enum TextBuffer {
    Ropey(RopeyRope),
    Lapce(LapceRope),
}

impl TextBuffer {
    fn open(candidate: Candidate, path: &Path) -> Result<Self, String> {
        match candidate {
            Candidate::Ropey => RopeyRope::from_reader(BufReader::with_capacity(
                1024 * 1024,
                File::open(path).map_err(display_error)?,
            ))
            .map(Self::Ropey)
            .map_err(display_error),
            Candidate::Lapce => stream_lapce(path).map(Self::Lapce),
        }
    }

    fn len_bytes(&self) -> usize {
        match self {
            Self::Ropey(rope) => rope.len_bytes(),
            Self::Lapce(rope) => rope.len(),
        }
    }

    fn floor_boundary(&self, offset: usize) -> usize {
        match self {
            Self::Ropey(rope) => rope.char_to_byte(rope.byte_to_char(offset)),
            Self::Lapce(rope) => rope.at_or_prev_codepoint_boundary(offset).unwrap_or(0),
        }
    }

    fn line_of_offset(&self, offset: usize) -> usize {
        match self {
            Self::Ropey(rope) => rope.byte_to_line(offset),
            Self::Lapce(rope) => rope.line_of_offset(offset),
        }
    }

    fn replace(&mut self, start: usize, end: usize, text: &str) {
        match self {
            Self::Ropey(rope) => {
                let start_char = rope.byte_to_char(start);
                let end_char = rope.byte_to_char(end);
                rope.remove(start_char..end_char);
                rope.insert(start_char, text);
            }
            Self::Lapce(rope) => rope.edit(start..end, text),
        }
    }

    fn checksum(&self) -> u64 {
        match self {
            Self::Ropey(rope) => rope.chunks().fold(FNV_OFFSET_BASIS, |hash, chunk| {
                fnv_update(hash, chunk.as_bytes())
            }),
            Self::Lapce(rope) => rope.iter_chunks(..).fold(FNV_OFFSET_BASIS, |hash, chunk| {
                fnv_update(hash, chunk.as_bytes())
            }),
        }
    }
}

fn stream_lapce(path: &Path) -> Result<LapceRope, String> {
    let mut input = BufReader::with_capacity(1024 * 1024, File::open(path).map_err(display_error)?);
    let mut builder: TreeBuilder<RopeInfo> = TreeBuilder::new();
    let mut block = vec![0_u8; 1024 * 1024];
    let mut pending = Vec::with_capacity(block.len() + 4);
    loop {
        let read = input.read(&mut block).map_err(display_error)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&block[..read]);
        match std::str::from_utf8(&pending) {
            Ok(text) => {
                builder.push_str(text);
                pending.clear();
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                let text = std::str::from_utf8(&pending[..valid]).map_err(display_error)?;
                builder.push_str(text);
                pending.drain(..valid);
            }
            Err(error) => return Err(format!("invalid UTF-8 at byte {}", error.valid_up_to())),
        }
    }
    let tail = std::str::from_utf8(&pending).map_err(display_error)?;
    builder.push_str(tail);
    Ok(builder.build())
}

struct Session {
    current: TextBuffer,
    undo: Vec<TextBuffer>,
    redo: Vec<TextBuffer>,
}

impl Session {
    fn new(current: TextBuffer) -> Self {
        Self {
            current,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    fn replace(&mut self, start: usize, end: usize, text: &str) {
        self.undo.push(self.current.clone());
        self.redo.clear();
        self.current.replace(start, end, text);
    }

    fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.current.clone());
        self.current = previous;
        true
    }

    fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.current.clone());
        self.current = next;
        true
    }
}

fn probe(candidate: Candidate, path: &Path, run_label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(display_error)?;
    let expected_size = usize::try_from(metadata.len()).map_err(display_error)?;
    let rss_before = process_value_kib("VmRSS:")?;
    let open_started = Instant::now();
    let buffer = TextBuffer::open(candidate, path)?;
    let open_elapsed = open_started.elapsed();
    if buffer.len_bytes() != expected_size {
        return Err(format!(
            "open length mismatch: expected {expected_size}, got {}",
            buffer.len_bytes()
        ));
    }
    let rss_open = process_value_kib("VmRSS:")?;
    let initial_checksum = buffer.checksum();
    let initial_len = buffer.len_bytes();
    let positions = [0, buffer.floor_boundary(initial_len / 2), initial_len];
    let line_probe = positions
        .iter()
        .map(|offset| buffer.line_of_offset(*offset))
        .collect::<Vec<_>>();

    let mut session = Session::new(buffer);
    let mut operation_times = Vec::with_capacity(28);
    for position in positions {
        timed_edit(
            &mut session,
            position,
            position,
            INSERT,
            &mut operation_times,
        );
        timed_edit(
            &mut session,
            position,
            position + INSERT.len(),
            REPLACEMENT,
            &mut operation_times,
        );
        timed_edit(
            &mut session,
            position,
            position + REPLACEMENT.len(),
            "",
            &mut operation_times,
        );
    }
    if session.current.len_bytes() != initial_len {
        return Err("insert/replace/delete cycle did not restore input length".to_owned());
    }

    for _ in 0..9 {
        if !session.undo() {
            return Err("undo history ended early".to_owned());
        }
    }
    if session.current.len_bytes() != initial_len {
        return Err("undo did not restore input length".to_owned());
    }
    for _ in 0..9 {
        if !session.redo() {
            return Err("redo history ended early".to_owned());
        }
    }
    if session.current.len_bytes() != initial_len {
        return Err("redo did not restore final edit-state length".to_owned());
    }
    if !session.undo() {
        return Err("final branch setup could not undo".to_owned());
    }
    let branch_at = session.current.len_bytes();
    timed_edit(
        &mut session,
        branch_at,
        branch_at,
        BRANCH,
        &mut operation_times,
    );
    if session.redo() {
        return Err("new edit branch did not clear redo history".to_owned());
    }

    let expected_checksum = fnv_update(
        fnv_update(initial_checksum, REPLACEMENT.as_bytes()),
        BRANCH.as_bytes(),
    );
    let expected_final_len = initial_len + REPLACEMENT.len() + BRANCH.len();
    let final_checksum = session.current.checksum();
    if session.current.len_bytes() != expected_final_len || final_checksum != expected_checksum {
        return Err("final size/checksum differs from streaming reference".to_owned());
    }

    operation_times.sort_unstable();
    let edit_total: Duration = operation_times.iter().copied().sum();
    let edit_p50 = percentile(&operation_times, 50);
    let edit_p95 = percentile(&operation_times, 95);
    let rss_edits = process_value_kib("VmRSS:")?;
    let peak_rss = process_value_kib("VmHWM:")?;
    let threads = process_value("Threads:")?;

    println!(
        "RESULT candidate={} run={} bytes={} initial_checksum={initial_checksum:016x} final_checksum={final_checksum:016x} open_ms={:.3} edit_total_us={} edit_p50_us={} edit_p95_us={} rss_before_kib={} rss_open_kib={} rss_edits_kib={} peak_rss_kib={} threads={} lines={:?}",
        candidate.name(),
        run_label,
        expected_size,
        open_elapsed.as_secs_f64() * 1000.0,
        edit_total.as_micros(),
        edit_p50.as_micros(),
        edit_p95.as_micros(),
        rss_before,
        rss_open,
        rss_edits,
        peak_rss,
        threads,
        line_probe,
    );
    Ok(())
}

fn timed_edit(
    session: &mut Session,
    start: usize,
    end: usize,
    text: &str,
    timings: &mut Vec<Duration>,
) {
    let started = Instant::now();
    session.replace(start, end, text);
    timings.push(started.elapsed());
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = (values.len() - 1) * percentile / 100;
    values[index]
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

fn process_value_kib(label: &str) -> Result<u64, String> {
    process_value(label)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn buffers(text: &str) -> [TextBuffer; 2] {
        [
            TextBuffer::Ropey(RopeyRope::from_str(text)),
            TextBuffer::Lapce(LapceRope::from(text)),
        ]
    }

    #[test]
    fn adapters_match_string_reference_for_unicode_edits() {
        let original = "ASCII Привет 🦀 e\u{301}\nnext line";
        for mut buffer in buffers(original) {
            let mut reference = original.to_owned();
            let positions = [0, original.find('🦀').unwrap(), original.len()];
            for raw_position in positions {
                let position = buffer.floor_boundary(raw_position);
                buffer.replace(position, position, INSERT);
                reference.insert_str(position, INSERT);
                buffer.replace(position, position + INSERT.len(), REPLACEMENT);
                reference.replace_range(position..position + INSERT.len(), REPLACEMENT);
                assert_eq!(buffer.len_bytes(), reference.len());
                assert_eq!(
                    buffer.checksum(),
                    fnv_update(FNV_OFFSET_BASIS, reference.as_bytes())
                );
                buffer.replace(position, position + REPLACEMENT.len(), "");
                reference.replace_range(position..position + REPLACEMENT.len(), "");
            }
            assert_eq!(
                buffer.checksum(),
                fnv_update(FNV_OFFSET_BASIS, original.as_bytes())
            );
        }
    }

    #[test]
    fn session_clears_redo_after_branch() {
        for buffer in buffers("Привет") {
            let mut session = Session::new(buffer);
            let end = session.current.len_bytes();
            session.replace(end, end, "!");
            assert!(session.undo());
            session.replace(0, 0, "*");
            assert!(!session.redo());
        }
    }

    #[test]
    fn generator_is_exact_and_deterministic() {
        let directory =
            env::temp_dir().join(format!("stillus-buffer-probe-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first.md");
        let second = directory.join("second.md");
        let checksum = generate_file(&first, 12_345).unwrap();
        assert_eq!(generate_file(&second, 12_345).unwrap(), checksum);
        assert_eq!(fs::metadata(&first).unwrap().len(), 12_345);
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }
}
