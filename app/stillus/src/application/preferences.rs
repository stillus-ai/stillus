// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::settings::{self, SettingsError, UiSettings, UiSettingsStore};
use std::{
    path::Path,
    sync::mpsc::{self, Receiver},
};

pub(crate) struct Load {
    pub store: Preferences,
    pub settings: UiSettings,
    pub diagnostic: Option<String>,
}
pub(crate) struct Preferences {
    store: Option<UiSettingsStore>,
    worker: Option<Receiver<(UiSettingsStore, Result<(), SettingsError>)>>,
    pending: Option<UiSettings>,
    due: Option<u64>,
    error: Option<String>,
    snapshot: UiSettings,
    revision: u64,
    persisted_revision: u64,
    worker_revision: u64,
    failed_revision: u64,
    now: u64,
}
impl Preferences {
    fn new(store: UiSettingsStore, snapshot: UiSettings) -> Self {
        Self {
            store: Some(store),
            worker: None,
            pending: None,
            due: None,
            error: None,
            snapshot,
            revision: 0,
            persisted_revision: 0,
            worker_revision: 0,
            failed_revision: 0,
            now: 0,
        }
    }
    pub(crate) fn unbound() -> Self {
        Self::new(UiSettingsStore::unbound(), UiSettings::default())
    }
    pub(crate) fn load(path: &Path) -> Load {
        let settings::SettingsLoad {
            store,
            settings,
            diagnostic,
        } = UiSettingsStore::load(path);
        Load {
            store: Self::new(store, settings.clone()),
            settings,
            diagnostic,
        }
    }
    pub(crate) fn snapshot(&self) -> &UiSettings {
        &self.snapshot
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn completion(
        &self,
        revision: u64,
    ) -> Option<Result<(), super::actions::ActionError>> {
        if self.persisted_revision >= revision {
            Some(Ok(()))
        } else {
            (self.failed_revision >= revision).then(|| {
                Err(super::actions::ActionError::Failed(
                    "settings save failed".into(),
                ))
            })
        }
    }
    pub(crate) fn stage(&mut self, settings: UiSettings) -> bool {
        if settings == self.snapshot {
            return false;
        }
        self.snapshot = settings.clone();
        let changed = if let Some(store) = self.store.as_mut() {
            store.stage(settings)
        } else if self.pending.as_ref() == Some(&settings) {
            false
        } else {
            self.pending = Some(settings);
            true
        };
        if changed {
            self.revision = self.revision.saturating_add(1);
            self.error = None;
            self.due = Some(self.now.saturating_add(250));
        }
        changed
    }
    pub(crate) fn poll(&mut self, now: u64) -> bool {
        self.now = now;
        let completion = self
            .worker
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        let changed = completion.is_some();
        if let Some((mut store, result)) = completion {
            self.worker = None;
            if result.is_ok() {
                self.persisted_revision = self.worker_revision;
            } else {
                self.failed_revision = self.worker_revision;
            }
            self.error = result.err().map(|error| error.to_string());
            if let Some(settings) = self.pending.take() {
                store.stage(settings);
            }
            self.store = Some(store);
        }
        if self.worker.is_none() && self.due.is_some_and(|due| due <= now) {
            self.due = None;
            if let Some(mut store) = self.store.take() {
                let (sender, receiver) = mpsc::sync_channel(1);
                self.worker = Some(receiver);
                self.worker_revision = self.revision;
                std::thread::spawn(move || {
                    let result = store.flush();
                    let _ = sender.send((store, result));
                });
            }
        }
        changed
    }
    /// Used at shutdown/session handoff after all content writers have settled.
    pub(crate) fn flush(&mut self) -> Result<(), SettingsError> {
        if let Some(receiver) = self.worker.take() {
            let (mut store, result) = receiver
                .recv()
                .map_err(|_| SettingsError::UnsafePath("settings worker stopped".into()))?;
            if let Some(settings) = self.pending.take() {
                store.stage(settings);
            }
            self.store = Some(store);
            result?;
        }
        self.due = None;
        self.store
            .as_mut()
            .expect("preferences owns store outside worker")
            .flush()?;
        self.persisted_revision = self.revision;
        Ok(())
    }
    pub(crate) fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}
