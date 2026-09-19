// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::sync::mpsc::SyncSender;
use stillus_core::{PersistenceCompletion, PersistenceJob};

pub(crate) fn start(
    job: PersistenceJob,
    sender: SyncSender<PersistenceCompletion>,
    lease: std::sync::Arc<stillus_platform::WorkspaceLease>,
) {
    std::thread::spawn(move || {
        let completion = job.execute();
        // A completion authorizes shutdown/restart, so release the worker's
        // ownership before publishing it. The live session still owns its lease.
        drop(lease);
        let _ = sender.send(completion);
    });
}
