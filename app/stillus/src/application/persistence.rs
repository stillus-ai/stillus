// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::sync::mpsc::SyncSender;
use stillus_core::{PersistenceCompletion, PersistenceJob};

pub(crate) fn start(job: PersistenceJob, sender: SyncSender<PersistenceCompletion>) {
    std::thread::spawn(move || {
        let _ = sender.send(job.execute());
    });
}
