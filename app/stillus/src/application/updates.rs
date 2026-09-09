// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

#[cfg(feature = "test-utils")]
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use stillus_update::{
    CheckMode, Decision, Installation, Release, UpdateError, UpdateTransport, Version,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Nothing has been checked yet in this session.
    Idle,
    Checking,
    UpToDate,
    Available(Box<Release>),
    /// Newer, but published too recently for an automatic check.
    Held(Box<Release>),
    /// Newer, without a package this platform can install.
    Unpackaged(Box<Release>),
    Downloading {
        release: Box<Release>,
        received: u64,
        total: Option<u64>,
    },
    Installed(Version),
    Failed(UpdateError),
    /// This build cannot replace itself, so only the message is shown.
    Unsupported(UpdateError),
}

impl Stage {
    pub(crate) fn release(&self) -> Option<&Release> {
        match self {
            Self::Available(release)
            | Self::Held(release)
            | Self::Unpackaged(release)
            | Self::Downloading { release, .. } => Some(release),
            _ => None,
        }
    }

    pub(crate) fn busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }
}

pub(crate) enum Message {
    Progress(u64, Option<u64>),
    Checked(Result<Decision, UpdateError>),
    Installed(Result<Version, UpdateError>),
}

pub(crate) fn start_check(
    transport: Box<dyn UpdateTransport>,
    current: Version,
    mode: CheckMode,
) -> Receiver<Message> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result =
            stillus_update::check(transport.as_ref(), current, mode, stillus_update::now_ms());
        let _ = sender.send(Message::Checked(result));
    });
    receiver
}

pub(crate) fn start_install(
    transport: Box<dyn UpdateTransport>,
    installation: Installation,
    release: Release,
) -> Receiver<Message> {
    let (sender, receiver) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        let version = release.version;
        let result = stillus_update::install(
            transport.as_ref(),
            &installation,
            &release,
            &mut |received, total| {
                // Progress is replaceable; the final outcome is always delivered.
                let _ = sender.try_send(Message::Progress(received, total));
            },
        );
        let _ = sender.send(Message::Installed(result.map(|()| version)));
    });
    receiver
}

pub(crate) struct Transition {
    pub stage: Stage,
    pub show_prompt: bool,
    pub completed: bool,
}
pub(crate) fn apply(
    previous: Stage,
    message: Message,
    automatic: bool,
    dismissed: Option<&str>,
) -> Transition {
    let mut show_prompt = false;
    let completed = !matches!(message, Message::Progress(..));
    let stage = match message {
        Message::Progress(received, total) => match previous {
            Stage::Downloading { release, .. } => Stage::Downloading {
                release,
                received,
                total,
            },
            stage => stage,
        },
        Message::Checked(Ok(decision)) => match decision {
            Decision::UpToDate => Stage::UpToDate,
            Decision::Available(release) => {
                show_prompt = automatic && dismissed != Some(release.version.to_string().as_str());
                Stage::Available(Box::new(release))
            }
            Decision::Held { release, .. } => Stage::Held(Box::new(release)),
            Decision::Unpackaged(release) => Stage::Unpackaged(Box::new(release)),
        },
        Message::Checked(Err(error)) | Message::Installed(Err(error)) => match error {
            UpdateError::NotInstalled | UpdateError::ReadOnly => Stage::Unsupported(error),
            error => Stage::Failed(error),
        },
        Message::Installed(Ok(version)) => {
            show_prompt = true;
            Stage::Installed(version)
        }
    };
    Transition {
        stage,
        show_prompt,
        completed,
    }
}

fn transport() -> Box<dyn UpdateTransport> {
    #[cfg(feature = "test-utils")]
    if let Ok(directory) = std::env::var("STILLUS_TEST_UPDATE") {
        return Box::new(fixtures::Directory::new(PathBuf::from(directory)));
    }
    Box::new(stillus_update::HttpsTransport)
}
fn installation() -> Option<Installation> {
    #[cfg(feature = "test-utils")]
    if let Ok(root) = std::env::var("STILLUS_TEST_UPDATE_ROOT") {
        return fixtures::installation(&PathBuf::from(root));
    }
    Installation::locate().ok()
}
#[cfg(feature = "test-utils")]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) struct Directory(PathBuf);

    impl Directory {
        pub(crate) fn new(path: PathBuf) -> Self {
            Self(path)
        }
    }

    impl UpdateTransport for Directory {
        fn fetch(
            &self,
            url: &str,
            _accept: &str,
            limit: u64,
            progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<Vec<u8>, UpdateError> {
            let name = if url.starts_with("https://api.github.com/") {
                "latest.json"
            } else {
                url.rsplit('/').next().unwrap_or_default()
            };
            if name.is_empty() || name.contains("..") {
                return Err(UpdateError::Response);
            }
            let bytes = std::fs::read(self.0.join(name)).map_err(|_| UpdateError::Network)?;
            if bytes.len() as u64 > limit {
                return Err(UpdateError::Response);
            }
            progress(bytes.len() as u64, Some(bytes.len() as u64));
            Ok(bytes)
        }
    }

    pub(crate) fn installation(root: &Path) -> Option<Installation> {
        if cfg!(target_os = "macos") {
            Installation::mac_app(root.join("Stillus.app")).ok()
        } else if cfg!(windows) {
            Installation::windows(root.join("Stillus.exe")).ok()
        } else {
            Installation::linux(root.join("stillus")).ok()
        }
    }
}

pub(crate) struct Coordinator {
    pub stage: Stage,
    pub prompt: bool,
    pub installation: Option<Installation>,
    receiver: Option<Receiver<Message>>,
    mode: CheckMode,
    startup_checked: bool,
}
impl Coordinator {
    pub(crate) fn new() -> Self {
        let installation = installation();
        if let Some(installation) = &installation {
            installation.cleanup();
        }
        Self {
            stage: if installation.is_some() {
                Stage::Idle
            } else {
                Stage::Unsupported(UpdateError::NotInstalled)
            },
            prompt: false,
            installation,
            receiver: None,
            mode: CheckMode::Manual,
            startup_checked: false,
        }
    }
    pub(crate) fn check(&mut self, mode: CheckMode) {
        if self.installation.is_none()
            || self.stage.busy()
            || matches!(self.stage, Stage::Installed(_))
        {
            return;
        }
        self.stage = Stage::Checking;
        self.mode = mode;
        let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0));
        self.receiver = Some(start_check(transport(), current, mode));
    }
    pub(crate) fn install(&mut self) {
        let Some(installation) = self.installation.clone() else {
            return;
        };
        let Stage::Available(release) = &self.stage else {
            return;
        };
        let release = (**release).clone();
        self.stage = Stage::Downloading {
            release: Box::new(release.clone()),
            received: 0,
            total: None,
        };
        self.mode = CheckMode::Manual;
        self.receiver = Some(start_install(transport(), installation, release));
    }
    pub(crate) fn poll(&mut self, now: u64, settings: &super::settings::UpdateSettings) -> bool {
        if !self.startup_checked && now >= 1500 {
            self.startup_checked = true;
            if settings.automatic {
                self.check(CheckMode::Automatic);
            }
        }
        let mut changed = false;
        loop {
            let Some(receiver) = self.receiver.as_ref() else {
                break;
            };
            match receiver.try_recv() {
                Ok(message) => {
                    let transition = apply(
                        self.stage.clone(),
                        message,
                        self.mode == CheckMode::Automatic,
                        settings.dismissed.as_deref(),
                    );
                    self.stage = transition.stage;
                    self.prompt |= transition.show_prompt;
                    changed = true;
                    if transition.completed {
                        self.receiver = None;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.receiver = None;
                    self.stage = Stage::Failed(UpdateError::Network);
                    changed = true;
                    break;
                }
            }
        }
        changed
    }
}
