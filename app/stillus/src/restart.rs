// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Explicit update restart. This is the only project-owned process launcher:
//! it starts the installed Stillus executable directly, without a shell, and
//! keeps the new UI from opening until the old UI has finished shutting down.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use stillus_update::{InstallKind, Installation};

pub(crate) const HANDOFF_FLAG: &str = "--restart-after-update";
const READY: &[u8] = b"stillus/restart/ready";

pub(crate) struct PendingRestart {
    child: Child,
    input: Option<ChildStdin>,
    committed: bool,
}

impl PendingRestart {
    pub(crate) fn start(installation: &Installation, workspace: Option<&Path>) -> io::Result<Self> {
        // Keep the original installation path: current_exe() can refer to a
        // retired or deleted executable after an update on Unix.
        let executable = match installation.kind() {
            InstallKind::MacApp => installation.target().join("Contents/MacOS/Stillus"),
            InstallKind::Linux | InstallKind::Windows => installation.target().to_path_buf(),
        };
        if !executable.is_absolute()
            || Installation::detect(&executable).as_ref() != Ok(installation)
        {
            return Err(io::Error::other("the updated installation is unavailable"));
        }
        let mut command = std::process::Command::new(executable);
        command.arg(HANDOFF_FLAG);
        if let Some(workspace) = workspace {
            command.arg("--workspace").arg(workspace);
        }
        Self::spawn(command)
    }

    fn spawn(mut command: std::process::Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take();
        Ok(Self {
            child,
            input,
            committed: false,
        })
    }

    /// Called only after the event loop and final settings flush complete.
    pub(crate) fn complete(mut self) -> io::Result<()> {
        self.input
            .as_mut()
            .ok_or_else(|| io::Error::other("restart pipe unavailable"))?
            .write_all(READY)?;
        self.input.take();
        self.committed = true;
        Ok(())
    }
}

impl Drop for PendingRestart {
    fn drop(&mut self) {
        if !self.committed {
            self.input.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub(crate) fn await_handoff() -> io::Result<bool> {
    read_handoff(io::stdin().lock())
}

fn read_handoff(input: impl Read) -> io::Result<bool> {
    // The protocol contains no note data and cannot allocate unbounded input.
    let mut message = Vec::new();
    input
        .take((READY.len() + 1) as u64)
        .read_to_end(&mut message)?;
    Ok(message == READY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            assert!(Instant::now() < deadline, "child handoff timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn child_command(root: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "restart::tests::child_handoff"])
            .env("STILLUS_RESTART_PROBE", root);
        command
    }

    #[test]
    fn child_handoff() {
        let Some(root) = std::env::var_os("STILLUS_RESTART_PROBE") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        std::fs::write(root.join("waiting"), b"").unwrap();
        if await_handoff().unwrap() {
            std::fs::write(root.join("started"), b"").unwrap();
        }
    }

    #[test]
    fn child_waits_for_completion_and_cancel_never_restarts() {
        let root = crate::test_support::workspace("stillus-restart");
        let pending = PendingRestart::spawn(child_command(&root)).unwrap();
        wait_for(&root.join("waiting"));
        assert!(!root.join("started").exists());
        pending.complete().unwrap();
        wait_for(&root.join("started"));

        std::fs::remove_file(root.join("waiting")).unwrap();
        std::fs::remove_file(root.join("started")).unwrap();
        let pending = PendingRestart::spawn(child_command(&root)).unwrap();
        wait_for(&root.join("waiting"));
        drop(pending);
        assert!(!root.join("started").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_executable_reports_an_error_before_shutdown() {
        let root = crate::test_support::workspace("stillus-restart-missing");
        assert!(PendingRestart::spawn(std::process::Command::new(root.join("missing"))).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handoff_requires_the_complete_token_and_eof() {
        assert!(read_handoff(READY).unwrap());
        assert!(!read_handoff(&b""[..]).unwrap());
        assert!(!read_handoff(&READY[..READY.len() - 1]).unwrap());
        assert!(!read_handoff([READY, b"x"].concat().as_slice()).unwrap());
        assert!(!read_handoff(io::repeat(b'x')).unwrap());
    }

    #[test]
    fn handoff_read_errors_are_reported() {
        struct Failure;
        impl Read for Failure {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("read failed"))
            }
        }
        assert!(read_handoff(Failure).is_err());
    }
}
