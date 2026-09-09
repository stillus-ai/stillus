// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::SEARCH_RECONCILE_MS;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};
use stillus_search::{MAX_RESULTS as MAX_SEARCH_RESULTS, SearchIndex, SearchResult};

pub(crate) struct SearchWorkerParts {
    pub(crate) sender: SyncSender<SearchCommand>,
    pub(crate) receiver: Receiver<SearchEvent>,
    pub(crate) worker: thread::JoinHandle<()>,
}

#[derive(Debug)]
pub(crate) enum SearchCommand {
    ToolQuery {
        query: String,
        reply: SyncSender<Result<Vec<SearchResult>, String>>,
    },
    Query {
        generation: u64,
        query: String,
    },
    Reconcile,
    Rebuild,
    SuspendAndPurge {
        paths: Vec<PathBuf>,
    },
    SuspendPasswordChange {
        operation_id: u64,
        paths: Vec<PathBuf>,
    },
    Resume,
    Purge {
        operation_id: u64,
        note_path: PathBuf,
    },
    RestoreAfterFailedPurge {
        operation_id: u64,
        note_path: PathBuf,
    },
    Shutdown(Sender<()>),
}

#[derive(Debug)]
pub(crate) enum SearchEvent {
    Indexing,
    Ready,
    Changed,
    Results {
        generation: u64,
        results: Vec<SearchResult>,
    },
    PurgeFinished {
        operation_id: u64,
        result: Result<(), String>,
    },
    RestoreFinished {
        operation_id: u64,
        result: Result<(), String>,
    },
    PasswordChangeSuspended {
        operation_id: u64,
    },
    Error(String),
}

pub(crate) fn spawn_search_worker(
    workspace: PathBuf,
    initially_suspended: bool,
) -> SearchWorkerParts {
    let (command_sender, command_receiver) = mpsc::sync_channel(64);
    let (event_sender, event_receiver) = mpsc::sync_channel(64);
    let worker = thread::spawn(move || {
        search_worker(
            workspace,
            command_receiver,
            event_sender,
            initially_suspended,
        );
    });
    SearchWorkerParts {
        sender: command_sender,
        receiver: event_receiver,
        worker,
    }
}

pub(crate) fn search_worker(
    workspace: PathBuf,
    commands: Receiver<SearchCommand>,
    events: SyncSender<SearchEvent>,
    initially_suspended: bool,
) {
    let _ = events.send(SearchEvent::Indexing);
    let mut index = None;
    let mut suspended = initially_suspended;
    if !suspended {
        index = match SearchIndex::open_or_rebuild(&workspace) {
            Ok(mut opened) => {
                match opened.reconcile() {
                    Ok(report) if report.added_or_updated > 0 || report.removed > 0 => {
                        let _ = events.send(SearchEvent::Changed);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = events.send(SearchEvent::Error(error.to_string()));
                    }
                }
                let _ = events.send(SearchEvent::Ready);
                Some(opened)
            }
            Err(error) => {
                let _ = events.send(SearchEvent::Error(error.to_string()));
                None
            }
        };
    }
    let mut last_reconcile = Instant::now();

    loop {
        let reconcile_interval = Duration::from_millis(SEARCH_RECONCILE_MS);
        let wait = reconcile_interval.saturating_sub(last_reconcile.elapsed());
        let mut batch = match commands.recv_timeout(wait) {
            Ok(command) => vec![command],
            Err(RecvTimeoutError::Timeout) => vec![SearchCommand::Reconcile],
            Err(RecvTimeoutError::Disconnected) => break,
        };
        batch.extend(commands.try_iter().take(63));
        if last_reconcile.elapsed() >= reconcile_interval
            && !batch
                .iter()
                .any(|command| matches!(command, SearchCommand::Reconcile))
        {
            batch.push(SearchCommand::Reconcile);
        }

        let mut latest_query = None;
        for command in batch {
            match command {
                SearchCommand::ToolQuery { query, reply } => {
                    let result = if suspended {
                        Err("search suspended".to_owned())
                    } else {
                        index
                            .as_ref()
                            .ok_or_else(|| "search unavailable".to_owned())
                            .and_then(|index| {
                                index
                                    .query(&query, MAX_SEARCH_RESULTS)
                                    .map_err(|error| error.to_string())
                            })
                    };
                    let _ = reply.try_send(result);
                }
                SearchCommand::Query { generation, query } => {
                    latest_query = Some((generation, query));
                }
                SearchCommand::Reconcile => {
                    last_reconcile = Instant::now();
                    if suspended {
                        continue;
                    }
                    let result = match &mut index {
                        Some(index) => index
                            .reconcile()
                            .map(|report| report.added_or_updated > 0 || report.removed > 0),
                        None => SearchIndex::open_or_rebuild(&workspace).map(|replacement| {
                            index = Some(replacement);
                            true
                        }),
                    };
                    match result {
                        Ok(true) => {
                            let _ = events.send(SearchEvent::Changed);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            let _ = events.send(SearchEvent::Error(error.to_string()));
                        }
                    }
                }
                SearchCommand::Rebuild => {
                    last_reconcile = Instant::now();
                    if suspended {
                        continue;
                    }
                    let _ = events.send(SearchEvent::Indexing);
                    let result = match &mut index {
                        Some(index) => index.rebuild(),
                        None => SearchIndex::open_or_rebuild(&workspace).map(|replacement| {
                            index = Some(replacement);
                        }),
                    };
                    match result {
                        Ok(()) => {
                            let _ = events.send(SearchEvent::Changed);
                            let _ = events.send(SearchEvent::Ready);
                        }
                        Err(error) => {
                            let _ = events.send(SearchEvent::Error(error.to_string()));
                        }
                    }
                }
                SearchCommand::SuspendAndPurge { paths } => {
                    suspended = true;
                    last_reconcile = Instant::now();
                    if let Some(index) = &mut index {
                        let mut changed = false;
                        for path in paths {
                            if index.purge(path).is_ok() {
                                changed = true;
                            }
                        }
                        if changed {
                            let _ = events.send(SearchEvent::Changed);
                        }
                    }
                }
                SearchCommand::SuspendPasswordChange {
                    operation_id,
                    paths,
                } => {
                    suspended = true;
                    last_reconcile = Instant::now();
                    if let Some(index) = &mut index {
                        let mut changed = false;
                        for path in paths {
                            if index.purge(path).is_ok() {
                                changed = true;
                            }
                        }
                        if changed {
                            let _ = events.send(SearchEvent::Changed);
                        }
                    }
                    let _ = events.send(SearchEvent::PasswordChangeSuspended { operation_id });
                }
                SearchCommand::Resume => {
                    suspended = false;
                    last_reconcile = Instant::now();
                    let result = match &mut index {
                        Some(index) => index
                            .reconcile()
                            .map(|report| report.added_or_updated > 0 || report.removed > 0),
                        None => SearchIndex::open_or_rebuild(&workspace).map(|replacement| {
                            index = Some(replacement);
                            true
                        }),
                    };
                    match result {
                        Ok(changed) => {
                            if changed {
                                let _ = events.send(SearchEvent::Changed);
                            }
                            let _ = events.send(SearchEvent::Ready);
                        }
                        Err(error) => {
                            let _ = events.send(SearchEvent::Error(error.to_string()));
                        }
                    }
                }
                SearchCommand::Purge {
                    operation_id,
                    note_path,
                } => {
                    last_reconcile = Instant::now();
                    let result = index
                        .as_mut()
                        .ok_or_else(|| "search unavailable".to_owned())
                        .and_then(|index| {
                            index.purge(note_path).map_err(|error| error.to_string())
                        });
                    let changed = result.is_ok();
                    let _ = events.send(SearchEvent::PurgeFinished {
                        operation_id,
                        result,
                    });
                    if changed {
                        let _ = events.send(SearchEvent::Changed);
                    }
                }
                SearchCommand::RestoreAfterFailedPurge {
                    operation_id,
                    note_path,
                } => {
                    last_reconcile = Instant::now();
                    let result = index
                        .as_mut()
                        .ok_or_else(|| "search unavailable".to_owned())
                        .and_then(|index| {
                            index
                                .restore_after_failed_purge(note_path)
                                .map_err(|error| error.to_string())
                        });
                    let changed = result.is_ok();
                    let _ = events.send(SearchEvent::RestoreFinished {
                        operation_id,
                        result,
                    });
                    if changed {
                        let _ = events.send(SearchEvent::Changed);
                    }
                }
                SearchCommand::Shutdown(finished) => {
                    let _ = finished.send(());
                    return;
                }
            }
        }

        if let Some((generation, query)) = latest_query
            && !suspended
        {
            let results = index
                .as_ref()
                .map(|index| index.query(&query, MAX_SEARCH_RESULTS))
                .transpose();
            match results {
                Ok(Some(results)) => {
                    let _ = events.send(SearchEvent::Results {
                        generation,
                        results,
                    });
                }
                Ok(None) => {
                    let _ = events.send(SearchEvent::Results {
                        generation,
                        results: Vec::new(),
                    });
                }
                Err(error) => {
                    let _ = events.send(SearchEvent::Error(error.to_string()));
                }
            }
        }
    }
}

/// Shutdown drains bounded events so joining cannot deadlock a worker publishing
/// its last result. Called only after the owner has stopped accepting UI actions.
pub(crate) fn shutdown(
    commands: &SyncSender<SearchCommand>,
    events: &Receiver<SearchEvent>,
    worker: thread::JoinHandle<()>,
) -> thread::Result<()> {
    let (finished, _) = mpsc::channel();
    let mut command = SearchCommand::Shutdown(finished);
    loop {
        match commands.try_send(command) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => break,
            Err(mpsc::TrySendError::Full(pending)) => {
                command = pending;
                for _ in events.try_iter() {}
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    while !worker.is_finished() {
        for _ in events.try_iter() {}
        thread::sleep(Duration::from_millis(1));
    }
    worker.join()
}
