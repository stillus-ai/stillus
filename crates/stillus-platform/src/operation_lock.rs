// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::diagnostics::{Operation, Stage, io_result};

/// An OS-held activity marker. Unlike a transaction lock it may cross threads and
/// lasts until the request completes; a crashed process releases it automatically.
pub struct ActivityLease {
    _file: File,
}

impl ActivityLease {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = super::create_private_file(path)?;
        fs4::fs_std::FileExt::lock_exclusive(&file)?;
        Ok(Self { _file: file })
    }

    pub fn is_held(path: &Path) -> io::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        super::validate_private(path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(3);
        }
        let file = options.open(path)?;
        if file.metadata()?.len() != 0 || super::file_information(&file)?.links != 1 {
            return Err(io::Error::other("invalid activity marker"));
        }
        Ok(!fs4::fs_std::FileExt::try_lock_exclusive(&file)?)
    }
}

thread_local! {
    static HELD: RefCell<HashMap<PathBuf, (File, usize)>> = RefCell::new(HashMap::new());
}

/// A short, reentrant transaction lock. The empty marker is never removed:
/// unlinking it would allow another process to lock a different inode.
pub struct OperationLock {
    directory: PathBuf,
    thread: PhantomData<Rc<()>>,
}

impl OperationLock {
    pub fn directory(directory: &Path) -> io::Result<Self> {
        let directory = directory.canonicalize()?;
        super::validate_real_path(&directory)?;
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some((_, depth)) = held.get_mut(&directory) {
                *depth += 1;
                return Ok(());
            }
            let path = directory.join(".stillus-operation.lock");
            // Publish an empty restricted marker before opening a shared handle.
            io_result(Operation::Lock, Stage::Create, prepare_marker(&path))?;
            io_result(
                Operation::Lock,
                Stage::Validate,
                super::validate_private(&path),
            )?;
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(3); // Readers/writers, never deletion while held.
            }
            let file = io_result(Operation::Lock, Stage::Open, options.open(&path))?;
            if file.metadata()?.len() != 0 || super::file_information(&file)?.links != 1 {
                return Err(io::Error::other("invalid operation lock marker"));
            }
            io_result(
                Operation::Lock,
                Stage::Acquire,
                fs4::fs_std::FileExt::lock_exclusive(&file),
            )?;
            held.insert(directory.clone(), (file, 1));
            Ok::<_, io::Error>(())
        })?;
        Ok(Self {
            directory,
            thread: PhantomData,
        })
    }

    /// Workspace notes share their root lock with recovery and password changes;
    /// unrelated external files use their containing directory.
    pub fn file(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = if parent.file_name().is_some_and(|name| name == "notes") {
            parent.parent().unwrap_or(parent)
        } else {
            parent
        };
        Self::directory(directory)
    }
}

#[cfg(unix)]
fn prepare_marker(path: &Path) -> io::Result<()> {
    match super::create_private_file(path) {
        Ok(file) => {
            drop(file);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn prepare_marker(path: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    // Existing markers are validated by the caller, never repaired or replaced.
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for _ in 0..32 {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_file_name(format!(
            ".stillus-operation-{}-{id}.tmp",
            std::process::id()
        ));
        let file = match super::create_private_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // ACL construction changes several rules. Only publish the final name
        // once the private ACL is complete and the exclusive handle is closed.
        drop(file);
        if let Err(error) = super::publish(&temporary, path) {
            // We own only this temporary, not the winning marker. Preserve a
            // publication error, but report failed cleanup after a lost race.
            let cleanup = std::fs::remove_file(&temporary);
            if error.kind() == io::ErrorKind::AlreadyExists {
                return cleanup;
            }
            return Err(error);
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate operation lock temporary",
    ))
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some((_, depth)) = held.get_mut(&self.directory) {
                *depth -= 1;
                if *depth == 0 {
                    held.remove(&self.directory);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn simultaneous_first_users_serialize_on_one_marker() {
        for round in 0..16 {
            simultaneous_first_users_round(round);
        }
    }

    fn simultaneous_first_users_round(round: usize) {
        let root = std::env::temp_dir().join(format!(
            "stillus-operation-first-{}-{round}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(4));
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let workers = (0..4)
            .map(|_| {
                let root = root.clone();
                let start = start.clone();
                let entered = entered.clone();
                std::thread::spawn(move || -> io::Result<_> {
                    start.wait();
                    let _lock = OperationLock::directory(&root)?;
                    assert_eq!(entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                    let identity = super::super::file_information(&File::open(
                        root.join(".stillus-operation.lock"),
                    )?)?
                    .identity;
                    std::thread::sleep(Duration::from_millis(20));
                    assert_eq!(entered.fetch_sub(1, std::sync::atomic::Ordering::SeqCst), 1);
                    Ok(identity)
                })
            })
            .collect::<Vec<_>>();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        eprintln!(
            "NATIVE_ASSERT operation=ConcurrentLock success={}",
            results.iter().all(Result::is_ok)
        );
        let identity = results[0].as_ref().unwrap();
        for result in &results {
            assert_eq!(identity, result.as_ref().unwrap());
        }
        let marker = root.join(".stillus-operation.lock");
        super::super::validate_private(&marker).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"");
        let entries = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![marker]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_invalid_marker_is_rejected_without_replacement() {
        let root =
            std::env::temp_dir().join(format!("stillus-operation-invalid-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let marker = root.join(".stillus-operation.lock");
        use std::io::Write;
        let mut file = super::super::create_private_file(&marker).unwrap();
        file.write_all(b"existing content").unwrap();
        let identity = super::super::file_information(&file).unwrap().identity;
        drop(file);
        assert!(OperationLock::directory(&root).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"existing content");
        assert_eq!(
            super::super::file_information(&File::open(&marker).unwrap())
                .unwrap()
                .identity,
            identity
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn serializes_independent_handles_and_allows_nested_operations() {
        let root =
            std::env::temp_dir().join(format!("stillus-operation-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let outer = OperationLock::directory(&root).unwrap();
        drop(OperationLock::directory(&root).unwrap());
        let (send, receive) = mpsc::channel();
        let other = root.clone();
        let worker = std::thread::spawn(move || {
            let _lock = OperationLock::directory(&other).unwrap();
            send.send(()).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(50)).is_err());
        drop(outer);
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
