// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use stillus_ai::{
    AiError, AiProvider,
    journal::{RequestJournal, RequestRecord, RequestStatus, now_ms},
};
use stillus_platform::{ActivityLease, OperationLock, fs};

const MAX_RECORD: u64 = 4 * 1024 * 1024;
const MAX_ACTIVE: usize = 4;
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_AGE: u64 = 30 * 24 * 60 * 60 * 1000;
pub(crate) const PAGE_SIZE: usize = 40;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static STORES: Mutex<Vec<Arc<FileJournal>>> = Mutex::new(Vec::new());

#[derive(Default)]
struct State {
    active: BTreeMap<String, ActivityLease>,
    pending: BTreeMap<String, RequestRecord>,
}

pub(crate) struct FileJournal {
    directory: PathBuf,
    state: Mutex<State>,
    max_bytes: u64,
    max_age: u64,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Summary {
    pub id: String,
    pub started_ms: u64,
    pub provider: Option<AiProvider>,
    pub status: RequestStatus,
    pub corrupt: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Filter {
    pub provider: Option<AiProvider>,
    pub status: Option<RequestStatus>,
}

impl FileJournal {
    pub(crate) fn for_home(home: &Path) -> Arc<Self> {
        let directory = home.join(".stillus/ai/journal");
        let mut stores = STORES.lock().expect("journal registry");
        if let Some(store) = stores.iter().find(|store| store.directory == directory) {
            return store.clone();
        }
        stores.retain(|store| Arc::strong_count(store) > 1 || store.blocked());
        let store = Arc::new(Self {
            directory,
            state: Mutex::new(State::default()),
            max_bytes: MAX_BYTES,
            max_age: MAX_AGE,
        });
        stores.push(store.clone());
        store
    }

    pub(crate) fn blocked(&self) -> bool {
        self.state
            .lock()
            .map_or(true, |state| !state.pending.is_empty())
    }

    fn prepare(&self) -> io::Result<()> {
        let home = self
            .directory
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("journal home unavailable"))?;
        stillus_platform::validate_real_path(home)?;
        let mut path = home.to_path_buf();
        for component in [".stillus", "ai", "journal"] {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Err(io::Error::other("journal requires a real directory")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match stillus_platform::create_private_directory(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            stillus_platform::validate_real_path(&path)?;
                        }
                        Err(error) => return Err(error),
                    }
                    stillus_platform::sync_directory(path.parent().expect("parent"))?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn path(&self, id: &str) -> io::Result<PathBuf> {
        if id.len() != 48 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::other("invalid journal id"));
        }
        Ok(self.directory.join(format!("{id}.json")))
    }

    fn lease_path(&self, id: &str) -> io::Result<PathBuf> {
        Ok(self.path(id)?.with_extension("active"))
    }

    fn active(&self, state: &State, id: &str) -> io::Result<bool> {
        Ok(state.active.contains_key(id) || ActivityLease::is_held(&self.lease_path(id)?)?)
    }

    fn remove(&self, id: &str) -> io::Result<()> {
        fs::remove_file(self.path(id)?)?;
        let lease = self.lease_path(id)?;
        if lease.try_exists()? {
            fs::remove_file(lease)?;
        }
        Ok(())
    }

    fn write(&self, record: &RequestRecord, initial: bool) -> io::Result<()> {
        struct Limited(Vec<u8>);
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.len() as u64 + bytes.len() as u64 > MAX_RECORD {
                    return Err(io::Error::other("journal record too large"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Limited(Vec::new());
        serde_json::to_writer(&mut bytes, record)?;
        let destination = self.path(&record.id)?;
        if !initial {
            stillus_platform::validate_private(&destination)?;
        }
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary =
            self.directory
                .join(format!(".{:x}.{:x}.tmp", std::process::id(), sequence));
        let result = (|| {
            let mut file = stillus_platform::create_private_file(&temporary)?;
            file.write_all(&bytes.0)?;
            file.sync_all()?;
            drop(file);
            if initial {
                stillus_platform::publish(&temporary, &destination)?;
                #[cfg(unix)]
                fs::remove_file(&temporary)?;
            } else {
                stillus_platform::replace(&temporary, &destination)?;
            }
            stillus_platform::sync_directory(&self.directory)
        })();
        if result.is_err() && temporary.exists() {
            fs::remove_file(&temporary)?;
        }
        result
    }

    fn read_disk(&self, id: &str) -> io::Result<RequestRecord> {
        let path = self.path(id)?;
        stillus_platform::validate_private(&path)?;
        let file = fs::File::open(path)?;
        if file.metadata()?.len() > MAX_RECORD {
            return Err(io::Error::other("journal record too large"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECORD {
            return Err(io::Error::other("journal record too large"));
        }
        let mut record: RequestRecord = serde_json::from_slice(&bytes)?;
        if record.version != 1 || record.id != id {
            return Err(io::Error::other("unsupported journal record"));
        }
        record.enforce_content_policy();
        Ok(record)
    }

    pub(crate) fn read(&self, id: &str) -> io::Result<RequestRecord> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("journal unavailable"))?;
        if let Some(record) = state.pending.get(id) {
            return Ok(record.clone());
        }
        let mut record = self.read_disk(id)?;
        if record.status == RequestStatus::Pending && !self.active(&state, id)? {
            record.status = RequestStatus::Unknown;
        }
        Ok(record)
    }

    /// Only one page of summaries and one bounded record are retained while scanning.
    pub(crate) fn list(&self, before: Option<&str>, filter: Filter) -> io::Result<Vec<Summary>> {
        if !self.directory.try_exists()? {
            return Ok(Vec::new());
        }
        stillus_platform::validate_real_path(&self.directory)?;
        let mut newest = BTreeMap::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if self.path(id).is_err() || before.is_some_and(|before| id >= before) {
                continue;
            }
            let record = self.read(id);
            let summary = match record {
                Ok(record) => Summary {
                    id: id.into(),
                    started_ms: record.started_ms,
                    provider: Some(record.provider),
                    status: record.status,
                    corrupt: false,
                },
                Err(_) => Summary {
                    id: id.into(),
                    started_ms: 0,
                    provider: None,
                    status: RequestStatus::Unknown,
                    corrupt: true,
                },
            };
            if filter
                .provider
                .as_ref()
                .is_some_and(|provider| summary.provider.as_ref() != Some(provider))
                || filter.status.is_some_and(|status| summary.status != status)
            {
                continue;
            }
            newest.insert(id.to_owned(), summary);
            if newest.len() > PAGE_SIZE {
                newest.pop_first();
            }
        }
        Ok(newest.into_values().rev().collect())
    }

    fn prune(&self, state: &State, reserve: u64, clear: bool) -> io::Result<()> {
        loop {
            let mut total = reserve;
            let mut oldest = BTreeMap::<String, u64>::new();
            let mut removed = false;
            let mut active = 0;
            for entry in fs::read_dir(&self.directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                    continue;
                };
                if self.path(id).is_err() {
                    continue;
                }
                let size = entry.metadata()?.len();
                total = total.saturating_add(size);
                if self.active(state, id)? {
                    active += 1;
                    total = total.saturating_add(MAX_RECORD.saturating_sub(size));
                    continue;
                }
                let Ok(record) = self.read_disk(id) else {
                    continue;
                };
                if clear || now_ms().saturating_sub(record.started_ms) >= self.max_age {
                    self.remove(id)?;
                    total = total.saturating_sub(size);
                    removed = true;
                } else {
                    oldest.insert(id.to_owned(), size);
                    if oldest.len() > 256 {
                        oldest.pop_last();
                    }
                }
            }
            if reserve > 0 && active >= MAX_ACTIVE {
                return Err(io::Error::other("journal request capacity exhausted"));
            }
            if total > self.max_bytes {
                for (id, size) in oldest {
                    self.remove(&id)?;
                    removed = true;
                    total = total.saturating_sub(size);
                    if total <= self.max_bytes {
                        break;
                    }
                }
            }
            if removed {
                stillus_platform::sync_directory(&self.directory)?;
            }
            if total <= self.max_bytes {
                return Ok(());
            }
            if !removed {
                return Err(io::Error::other("journal capacity exhausted"));
            }
        }
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        if !self.directory.try_exists()? {
            return Ok(());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("journal unavailable"))?;
        let _lock = OperationLock::directory(&self.directory)?;
        self.prune(&state, 0, true)
    }

    pub(crate) fn retry(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("journal unavailable"))?;
        self.prepare()?;
        let _lock = OperationLock::directory(&self.directory)?;
        while let Some((id, record)) = state.pending.first_key_value() {
            self.write(record, false)?;
            let id = id.clone();
            state.pending.remove(&id);
            state.active.remove(&id);
        }
        self.prune(&state, 0, false)
    }
}

impl RequestJournal for FileJournal {
    fn begin(&self, mut record: RequestRecord) -> Result<RequestRecord, AiError> {
        let mut state = self.state.lock().map_err(|_| AiError::Journal)?;
        if !state.pending.is_empty() || state.active.len() >= MAX_ACTIVE {
            return Err(AiError::Journal);
        }
        self.prepare().map_err(|_| AiError::Journal)?;
        let _lock = OperationLock::directory(&self.directory).map_err(|_| AiError::Journal)?;
        self.prune(&state, MAX_RECORD, false)
            .map_err(|_| AiError::Journal)?;
        record.id = format!(
            "{:016x}{:016x}{:016x}",
            now_ms(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        record.enforce_content_policy();
        let lease_path = self.lease_path(&record.id).map_err(|_| AiError::Journal)?;
        let lease = ActivityLease::create(&lease_path).map_err(|_| AiError::Journal)?;
        self.write(&record, true).map_err(|_| AiError::Journal)?;
        state.active.insert(record.id.clone(), lease);
        Ok(record)
    }

    fn complete(&self, mut record: RequestRecord) {
        record.enforce_content_policy();
        let mut state = self.state.lock().expect("journal state");
        if !state.active.contains_key(&record.id) {
            return;
        }
        let result = self.prepare().and_then(|()| {
            let _lock = OperationLock::directory(&self.directory)?;
            self.write(&record, false)
        });
        if result.is_err() {
            state.pending.insert(record.id.clone(), record);
        } else {
            state.active.remove(&record.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use stillus_ai::{
        AiModel, ApiKey, CatalogTransport,
        journal::{ContentPolicy, safe_response},
    };

    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "stillus-journal-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> FileJournal {
            FileJournal {
                directory: self.0.join(".stillus/ai/journal"),
                state: Mutex::new(State::default()),
                max_bytes: MAX_BYTES,
                max_age: MAX_AGE,
            }
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn record() -> RequestRecord {
        RequestRecord::catalog(AiProvider::OpenAi, "ai/catalog/test", None)
    }
    fn completed(store: &FileJournal) -> String {
        let mut record = store.begin(record()).unwrap();
        record.status = RequestStatus::Success;
        record.response = Some(serde_json::json!({"data":[]}));
        let id = record.id.clone();
        store.complete(record);
        id
    }

    #[test]
    fn journal_persists_pages_filters_details_unknown_and_corrupt_records() {
        let home = Home::new();
        let store = home.store();
        let pending = store.begin(record()).unwrap();
        assert_eq!(
            store.read(&pending.id).unwrap().status,
            RequestStatus::Pending
        );
        for _ in 0..43 {
            completed(&store);
        }
        let newest = store.list(None, Filter::default()).unwrap();
        assert_eq!(newest.len(), PAGE_SIZE);
        let older = store
            .list(Some(&newest.last().unwrap().id), Filter::default())
            .unwrap();
        assert_eq!(older.len(), 4);
        assert!(newest.iter().all(|a| older.iter().all(|b| a.id != b.id)));
        let fresh = home.store();
        assert_eq!(
            fresh.read(&pending.id).unwrap().status,
            RequestStatus::Pending
        );
        // Independent journal instances (including other processes) cannot clear a live request.
        drop(store.state.lock().unwrap().active.remove(&pending.id));
        assert_eq!(
            fresh.read(&pending.id).unwrap().status,
            RequestStatus::Unknown
        );
        let path = store.path(&newest[0].id).unwrap();
        fs::write(&path, b"broken").unwrap();
        assert!(
            store
                .list(None, Filter::default())
                .unwrap()
                .iter()
                .any(|row| row.corrupt)
        );
        assert_eq!(
            store
                .list(
                    None,
                    Filter {
                        provider: Some(AiProvider::Anthropic),
                        status: None
                    }
                )
                .unwrap()
                .len(),
            0
        );
        assert!(store.read("../notes").is_err());
    }

    #[test]
    fn journal_failure_blocks_network_and_retains_completion_without_resending() {
        struct Catalog(AtomicU64);
        impl CatalogTransport for Catalog {
            fn list(&self, _: AiProvider, _: &ApiKey) -> Result<Vec<AiModel>, AiError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(AiError::Unauthorized)
            }
        }
        let home = Home::new();
        let store = home.store();
        let catalog = Catalog(AtomicU64::new(0));
        let (_, key) = ApiKey::parse(zeroize::Zeroizing::new(
            "sk-proj-abcdefghijklmnopqrstuv".into(),
        ))
        .unwrap();
        fs::write(home.0.join(".stillus"), b"preserve").unwrap();
        assert_eq!(
            catalog.list_recorded(AiProvider::OpenAi, &key, &store, "test"),
            Err(AiError::Journal)
        );
        assert_eq!(catalog.0.load(Ordering::Relaxed), 0);
        fs::remove_file(home.0.join(".stillus")).unwrap();
        assert_eq!(
            catalog.list_recorded(AiProvider::OpenAi, &key, &store, "test"),
            Err(AiError::Unauthorized)
        );
        let mut pending = store.begin(record()).unwrap();
        let path = store.path(&pending.id).unwrap();
        let original = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        pending.status = RequestStatus::Success;
        pending.response = Some(serde_json::json!({"result":"retained"}));
        store.complete(pending.clone());
        assert!(store.blocked());
        assert!(matches!(store.begin(record()), Err(AiError::Journal)));
        assert_eq!(store.read(&pending.id).unwrap().response, pending.response);
        fs::remove_dir(&path).unwrap();
        let mut file = stillus_platform::create_private_file(&path).unwrap();
        file.write_all(&original).unwrap();
        drop(file);
        store.retry().unwrap();
        assert!(!store.blocked());
        assert_eq!(
            store.read_disk(&pending.id).unwrap().response,
            pending.response
        );
        assert_eq!(catalog.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn journal_never_persists_keys_or_protected_payloads_and_rejects_symlinks() {
        let home = Home::new();
        let store = home.store();
        let (_, key) = ApiKey::parse(zeroize::Zeroizing::new(
            "sk-proj-abcdefghijklmnopqrstuv".into(),
        ))
        .unwrap();
        let raw = format!(
            "{{\"message\":\"{}\",\"password\":\"secret\",\"escaped\":\"sk\\u002dproj-abcdefghijklmnopqrstuv\"}}",
            key.expose()
        );
        let safe = safe_response(raw.as_bytes(), &key).to_string();
        assert!(!safe.contains(key.expose()));
        assert!(!safe.contains("secret"));
        let mut protected = record();
        protected.content_policy = ContentPolicy::Protected;
        protected.request = Some(serde_json::json!("protected body"));
        protected.parameters = serde_json::json!("protected body");
        let mut protected = store.begin(protected).unwrap();
        protected.response = Some(serde_json::json!("protected body"));
        protected.error = Some("protected body".into());
        let id = protected.id.clone();
        store.complete(protected);
        assert!(
            !String::from_utf8(fs::read(store.path(&id).unwrap()).unwrap())
                .unwrap()
                .contains("protected body")
        );
        #[cfg(unix)]
        {
            let other = Home::new();
            std::os::unix::fs::symlink(&home.0, other.0.join(".stillus")).unwrap();
            assert!(matches!(
                other.store().begin(record()),
                Err(AiError::Journal)
            ));
        }
    }

    #[test]
    fn journal_retention_enforces_age_capacity_and_keeps_active_requests() {
        let home = Home::new();
        let mut store = home.store();
        let id = completed(&store);
        let mut old = store.read_disk(&id).unwrap();
        old.started_ms = 1;
        store.write(&old, false).unwrap();
        let active = store.begin(record()).unwrap();
        assert!(!store.path(&id).unwrap().exists());
        let finished = completed(&store);
        store.max_bytes = MAX_RECORD;
        assert!(matches!(store.begin(record()), Err(AiError::Journal)));
        assert!(store.path(&active.id).unwrap().exists());
        assert!(!store.path(&finished).unwrap().exists());
        store.max_bytes = MAX_BYTES;
        home.store().clear().unwrap();
        assert!(store.path(&active.id).unwrap().exists());
    }

    #[test]
    fn journal_concurrent_requests_keep_unique_ids_and_bounded_reservations() {
        let home = Home::new();
        let store = Arc::new(home.store());
        std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|_| {
                    let store = store.clone();
                    scope.spawn(move || completed(&store))
                })
                .collect::<Vec<_>>();
            let ids = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<BTreeSet<_>>();
            assert_eq!(ids.len(), 4);
        });
        let requests = (0..MAX_ACTIVE)
            .map(|_| store.begin(record()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(store.begin(record()), Err(AiError::Journal)));
        for mut request in requests {
            request.status = RequestStatus::Error;
            store.complete(request);
        }
        assert!(!store.blocked());
    }
}
