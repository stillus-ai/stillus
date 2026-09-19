// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

/// Exclusive application session. The marker is permanent; only the OS lock
/// determines ownership. Closing the last handle (including on crash) releases it.
#[derive(Debug)]
pub struct WorkspaceLease {
    _file: File,
}

impl WorkspaceLease {
    /// `None` means a live owner holds the lease; errors must never be treated as
    /// permission to open the workspace. This lock is intentionally not reentrant.
    pub fn try_acquire(root: &Path) -> io::Result<Option<Self>> {
        let root = root.canonicalize()?;
        super::validate_real_path(&root)?;
        let path = root.join(".stillus-session.lock");
        super::operation_lock::prepare_marker(&path)?;
        super::validate_private(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(3).custom_flags(0x0020_0000);
        }
        let file = options.open(&path)?;
        let info = super::file_information(&file)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() != 0 || info.links != 1 {
            return Err(io::Error::other("invalid workspace lease marker"));
        }
        if !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
            return Ok(None);
        }
        super::validate_private(&path)?;
        if super::file_information(&File::open(&path)?)?.identity != info.identity {
            return Err(io::Error::other(
                "workspace lease marker changed during acquisition",
            ));
        }
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "stillus-lease-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn exclusive_until_last_owner_drops_and_marker_is_reusable() {
        let f = Fixture::new();
        let other = Fixture::new();
        let lease = Arc::new(WorkspaceLease::try_acquire(&f.0).unwrap().unwrap());
        let identity =
            super::super::file_information(&File::open(f.0.join(".stillus-session.lock")).unwrap())
                .unwrap()
                .identity;
        assert!(
            WorkspaceLease::try_acquire(&f.0.join("."))
                .unwrap()
                .is_none()
        );
        assert!(WorkspaceLease::try_acquire(&other.0).unwrap().is_some());
        let worker_owner = lease.clone();
        drop(lease);
        std::thread::sleep(Duration::from_millis(50));
        assert!(WorkspaceLease::try_acquire(&f.0).unwrap().is_none());
        drop(worker_owner);
        assert!(WorkspaceLease::try_acquire(&f.0).unwrap().is_some());
        assert_eq!(
            super::super::file_information(&File::open(f.0.join(".stillus-session.lock")).unwrap())
                .unwrap()
                .identity,
            identity
        );
    }

    #[test]
    fn simultaneous_first_open_has_exactly_one_owner() {
        let f = Fixture::new();
        let barrier = Arc::new(Barrier::new(4));
        let workers = (0..4)
            .map(|_| {
                let path = f.0.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let lease = WorkspaceLease::try_acquire(&path).unwrap();
                    barrier.wait();
                    lease.is_some()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| usize::from(worker.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn invalid_marker_is_preserved() {
        use std::io::Write;
        let f = Fixture::new();
        let path = f.0.join(".stillus-session.lock");
        super::super::create_private_file(&path)
            .unwrap()
            .write_all(b"foreign")
            .unwrap();
        assert!(WorkspaceLease::try_acquire(&f.0).is_err());
        assert_eq!(fs::read(path).unwrap(), b"foreign");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_marker_is_rejected() {
        let f = Fixture::new();
        let other = Fixture::new();
        super::super::create_private_file(&other.0.join("target")).unwrap();
        std::os::unix::fs::symlink(other.0.join("target"), f.0.join(".stillus-session.lock"))
            .unwrap();
        assert!(WorkspaceLease::try_acquire(&f.0).is_err());
    }

    // Driven by tools/test_workspace_lease.py so production Rust gains no launcher.
    #[test]
    fn process_probe() {
        let Some(root) = std::env::var_os("STILLUS_LEASE_PROBE") else {
            return;
        };
        let root = PathBuf::from(root);
        let lease = WorkspaceLease::try_acquire(&root).unwrap();
        match std::env::var("STILLUS_LEASE_MODE").unwrap().as_str() {
            "busy" => assert!(lease.is_none()),
            "free" => assert!(lease.is_some()),
            "hold" => {
                assert!(lease.is_some());
                fs::write(root.join("ready"), b"").unwrap();
                while !root.join("release").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            _ => panic!("unknown lease probe mode"),
        }
    }
}
