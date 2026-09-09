// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Test-only deadlines and workspace allocation, independent of wall clocks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub(crate) fn workspace(prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated test workspace: {error}"),
        }
    }
}

pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn new() -> Self {
        Self(Instant::now() + Duration::from_secs(60))
    }

    pub(crate) fn receive<T>(&self, receiver: &Receiver<T>) -> Result<T, RecvTimeoutError> {
        self.receive_at(receiver, Instant::now())
    }

    fn receive_at<T>(&self, receiver: &Receiver<T>, now: Instant) -> Result<T, RecvTimeoutError> {
        let remaining = self.0.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(RecvTimeoutError::Timeout);
        }
        receiver.recv_timeout(remaining)
    }

    pub(crate) fn matching<T, R>(
        &self,
        receiver: &Receiver<T>,
        mut select: impl FnMut(T) -> Option<R>,
    ) -> Result<R, RecvTimeoutError> {
        loop {
            if let Some(result) = select(self.receive(receiver)?) {
                return Ok(result);
            }
        }
    }
}

#[test]
fn workspaces_are_unique_without_a_clock() {
    let paths = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..16)
            .map(|_| scope.spawn(|| workspace("stillus-concurrent-fixture")))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        16
    );
    for path in paths {
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        std::fs::remove_dir(path).unwrap();
    }
}

#[test]
fn matching_ignores_progress_and_other_operations() {
    let (sender, receiver) = std::sync::mpsc::channel();
    for event in [(7, None), (6, Some(1)), (7, Some(42))] {
        sender.send(event).unwrap();
    }
    assert_eq!(
        Deadline::new().matching(&receiver, |(id, value)| {
            if id == 7 { value } else { None }
        }),
        Ok(42)
    );
}

#[test]
fn disconnected_workers_fail_without_waiting() {
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    drop(sender);
    assert_eq!(
        Deadline::new().receive(&receiver),
        Err(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn intermediate_events_never_extend_the_deadline() {
    let start = Instant::now();
    let deadline = Deadline(start + Duration::from_secs(60));
    let (sender, receiver) = std::sync::mpsc::channel();
    for elapsed in [0, 30, 59] {
        sender.send(elapsed).unwrap();
        assert_eq!(
            deadline.receive_at(&receiver, start + Duration::from_secs(elapsed)),
            Ok(elapsed)
        );
    }
    sender.send(60).unwrap();
    assert_eq!(
        deadline.receive_at(&receiver, start + Duration::from_secs(60)),
        Err(RecvTimeoutError::Timeout)
    );
}
