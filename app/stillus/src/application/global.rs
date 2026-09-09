// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Global coordinators survive workspace changes. Only the owner polls their workers.
use super::{
    ai,
    journal::{FileJournal, Filter, Summary},
    settings::{self, GlobalSettings, GlobalSettingsStore, SettingsError, UpdateSettings},
    updates,
};
use crate::i18n::Locale;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
};
use stillus_ai::{AiSettings, journal::RequestRecord};

pub(crate) struct GlobalLoad {
    pub store: GlobalApplication,
    pub settings: GlobalSettings,
    pub diagnostic: Option<String>,
}

type AiWorker = (
    u64,
    Receiver<Result<AiSettings, ai::Failure>>,
    Arc<AtomicBool>,
);

pub(crate) struct GlobalApplication {
    store: GlobalSettingsStore,
    next: u64,
    ai_worker: Option<AiWorker>,
    ai_results: BTreeMap<u64, Result<AiSettings, ai::Failure>>,
    journal_worker: Option<(u64, Receiver<Result<JournalPage, String>>)>,
    journal_results: BTreeMap<u64, Result<JournalPage, String>>,
    pub(crate) update: updates::Coordinator,
}

#[derive(Clone)]
pub(crate) struct JournalPage {
    pub rows: Vec<Summary>,
    pub detail: Option<RequestRecord>,
    pub blocked: bool,
}
pub(crate) struct JournalRequest {
    pub before: Option<String>,
    pub filter: Filter,
    pub selected: Option<String>,
    pub clear: bool,
    pub retry: bool,
}

impl GlobalApplication {
    pub(crate) fn load(home: Option<&Path>) -> GlobalLoad {
        let settings::GlobalSettingsLoad {
            store,
            settings,
            diagnostic,
        } = GlobalSettingsStore::load(home);
        GlobalLoad {
            store: Self {
                store,
                next: 0,
                ai_worker: None,
                ai_results: BTreeMap::new(),
                journal_worker: None,
                journal_results: BTreeMap::new(),
                update: updates::Coordinator::new(),
            },
            settings,
            diagnostic,
        }
    }
    pub(crate) fn reload(&mut self) -> Option<String> {
        let loaded = GlobalSettingsStore::load(self.store.home().as_deref());
        self.store = loaded.store;
        loaded.diagnostic
    }
    pub(crate) fn home(&self) -> Option<PathBuf> {
        self.store.home()
    }
    pub(crate) fn ai(&self) -> AiSettings {
        self.store.ai()
    }
    pub(crate) fn updates(&self) -> UpdateSettings {
        self.store.updates()
    }
    pub(crate) fn locale(&self) -> Locale {
        self.store.locale()
    }
    pub(crate) fn public_settings(&self) -> settings::PublicSettings {
        self.store.public_settings()
    }
    pub(crate) fn change_public(
        &mut self,
        edit: settings::PublicEdit,
    ) -> Result<(), SettingsError> {
        self.store.change_public(edit)
    }
    pub(crate) fn set_locale(&mut self, locale: Locale) -> Result<(), SettingsError> {
        self.store.set_locale(locale)
    }
    pub(crate) fn set_updates(&mut self, value: UpdateSettings) -> Result<(), SettingsError> {
        self.store.set_updates(value)
    }
    pub(crate) fn remember_workspace_from_ui(&mut self, path: &Path) -> Result<(), SettingsError> {
        self.store.remember_workspace(path)
    }
    fn next(&mut self) -> u64 {
        self.next = self.next.saturating_add(1);
        self.next
    }
    pub(crate) fn start_ai(
        &mut self,
        expected: AiSettings,
        action: ai::Action,
    ) -> Result<u64, ai::Failure> {
        if self.ai_worker.is_some() {
            return Err(ai::Failure::Settings);
        }
        let home = self.home().ok_or(ai::Failure::Settings)?;
        let id = self.next();
        let cancel = Arc::new(AtomicBool::new(false));
        let receiver = ai::start(home, expected, action, cancel.clone());
        self.ai_worker = Some((id, receiver, cancel));
        Ok(id)
    }
    pub(crate) fn cancel_ai(&mut self) {
        if let Some((_, _, cancel)) = &self.ai_worker {
            cancel.store(true, Ordering::Release);
        }
    }
    pub(crate) fn ai_result(&self, id: u64) -> Option<Result<AiSettings, ai::Failure>> {
        self.ai_results.get(&id).cloned()
    }
    pub(crate) fn ai_busy(&self) -> bool {
        self.ai_worker.is_some()
    }
    pub(crate) fn journal_blocked(&self) -> bool {
        self.home()
            .is_some_and(|home| FileJournal::for_home(&home).blocked())
    }
    pub(crate) fn start_journal(&mut self, request: JournalRequest) -> Result<u64, String> {
        if self.journal_worker.is_some() {
            return Err("journal is busy".into());
        }
        let home = self.home().ok_or("journal is unavailable")?;
        let store = FileJournal::for_home(&home);
        let id = self.next();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| -> std::io::Result<JournalPage> {
                if request.retry {
                    store.retry()?;
                }
                if request.clear {
                    store.clear()?;
                }
                Ok(JournalPage {
                    rows: store.list(request.before.as_deref(), request.filter)?,
                    detail: request.selected.map(|id| store.read(&id)).transpose()?,
                    blocked: store.blocked(),
                })
            })()
            .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        self.journal_worker = Some((id, receiver));
        Ok(id)
    }
    pub(crate) fn journal_result(&self, id: u64) -> Option<Result<JournalPage, String>> {
        self.journal_results.get(&id).cloned()
    }
    pub(crate) fn poll(&mut self, now: u64) -> bool {
        let mut changed = false;
        let ai = self
            .ai_worker
            .as_ref()
            .and_then(|(id, receiver, _)| match receiver.try_recv() {
                Ok(result) => Some((*id, result)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some((*id, Err(ai::Failure::Api(stillus_ai::AiError::Network))))
                }
            });
        if let Some((id, result)) = ai {
            self.ai_worker = None;
            // A cancellation can race a successful settings commit. Always reload disk.
            self.reload();
            self.ai_results.insert(id, result);
            while self.ai_results.len() > 64 {
                self.ai_results.pop_first();
            }
            changed = true;
        }
        let journal =
            self.journal_worker
                .as_ref()
                .and_then(|(id, receiver)| match receiver.try_recv() {
                    Ok(result) => Some((*id, result)),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some((*id, Err("journal worker stopped".into())))
                    }
                });
        if let Some((id, result)) = journal {
            self.journal_worker = None;
            self.journal_results.insert(id, result);
            // At most two bounded page results retain response contents.
            while self.journal_results.len() > 2 {
                self.journal_results.pop_first();
            }
            changed = true;
        }
        changed |= self.update.poll(now, &self.store.updates());
        changed
    }
}
