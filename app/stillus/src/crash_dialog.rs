// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::i18n::{self, msg};
use std::backtrace::Backtrace;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct Report {
    summary: String,
    text: String,
}

impl Report {
    // Accept only source locations, never panic payloads or application state.
    fn capture(location: Option<&std::panic::Location<'_>>) -> Self {
        let location = location.map_or_else(
            || "unknown location".to_owned(),
            |location| location.to_string(),
        );
        let summary =
            msg!(CrashSummary, "location" => location.clone()).render_for(i18n::crash_locale());
        let technical_summary = format!("An error occurred in Stillus.\nLocation: {location}");
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let backtrace = Backtrace::force_capture();
        let text = format!(
            "\n=== Stillus panic: unix={timestamp} pid={} ===\n\
             Version: {}\nPlatform: {} / {}\nThread: {:?}\n\
             {technical_summary}\nPanic payload omitted for privacy.\nBacktrace:\n{backtrace}\n",
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::current().id(),
        );
        Self { summary, text }
    }
}

fn enter_once(active: &AtomicBool) -> bool {
    !active.swap(true, Ordering::SeqCst)
}

fn append_report(path: &Path, report: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(report.as_bytes())?;
    file.flush()?;
    file.sync_data()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(target_os = "macos", windows, test))]
enum Choice {
    Copy,
    Close,
}

// The dialog and clipboard are injectable so tests never open native UI or exit.
#[cfg(any(target_os = "macos", windows, test))]
fn present_report(
    report: &Report,
    mut show: impl FnMut(&str, bool) -> Choice,
    mut copy: impl FnMut(&str) -> Result<(), ()>,
) {
    let mut copy_failed = false;
    while show(&report.summary, copy_failed) == Choice::Copy {
        if copy(&report.text).is_ok() {
            return;
        }
        copy_failed = true;
    }
}

fn log_and_present(
    path: &Path,
    report: &Report,
    present: impl FnOnce(&Report),
) -> std::io::Result<()> {
    let logged = append_report(path, &report.text);
    present(report);
    logged
}

pub(super) fn install() {
    let error_log = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("error.log");
    let active = AtomicBool::new(false);
    std::panic::set_hook(Box::new(move |info| {
        if !enter_once(&active) {
            std::process::exit(1);
        }
        let report = Report::capture(info.location());
        // Do not invoke the default hook: it prints arbitrary panic payloads.
        let _ = std::io::stderr().write_all(report.text.as_bytes());
        let _ = log_and_present(&error_log, &report, |report| {
            #[cfg(any(target_os = "macos", windows))]
            present_report(report, show_native, copy_native);
            #[cfg(target_os = "linux")]
            linux::present(report);
        });
        // Never resume the damaged event loop or run editor shutdown/autosave.
        std::process::exit(1);
    }));
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use gtk4::{gdk, gio, glib, prelude::*};
    use std::sync::mpsc;
    use std::time::Duration;

    pub(super) fn present(report: &Report) {
        let summary = report.summary.clone();
        let text = report.text.clone();
        let locale = i18n::crash_locale();
        let (started, ready) = mpsc::sync_channel(1);
        let (closed, done) = mpsc::sync_channel(1);
        // This independent GTK loop works even when the Floem thread panics.
        let spawned = std::thread::Builder::new().spawn(move || {
            if gtk4::init().is_err() {
                return;
            }
            let Some(display) = gdk::Display::default() else {
                return;
            };
            let loop_ = glib::MainLoop::new(None, false);
            let window = gtk4::Window::builder()
                .title(msg!(CrashTitle).render_for(locale))
                .default_width(480)
                .resizable(false)
                .build();
            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
            content.set_margin_top(24);
            content.set_margin_bottom(24);
            content.set_margin_start(24);
            content.set_margin_end(24);
            let label = gtk4::Label::new(Some(&summary));
            label.set_wrap(true);
            label.set_selectable(true);
            content.append(&label);
            let feedback = gtk4::Label::new(None);
            feedback.set_wrap(true);
            content.append(&feedback);
            let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            let copy = gtk4::Button::with_label(&msg!(CopyTrace).render_for(locale));
            let close = gtk4::Button::with_label(&msg!(Close).render_for(locale));
            let clipboard = display.clipboard();
            copy.connect_clicked(move |_| {
                clipboard.set_text(&text);
                let expected = text.clone();
                let feedback = feedback.clone();
                clipboard.read_text_async(None::<&gio::Cancellable>, move |result| {
                    let copied = result
                        .ok()
                        .flatten()
                        .is_some_and(|actual| actual.as_str() == expected);
                    let message = if copied {
                        String::new()
                    } else {
                        msg!(CopyTraceFailed).render_for(locale)
                    };
                    feedback.set_text(&message);
                });
            });
            let close_window = window.clone();
            close.connect_clicked(move |_| close_window.close());
            buttons.append(&copy);
            buttons.append(&close);
            content.append(&buttons);
            window.set_child(Some(&content));
            let closing_loop = loop_.clone();
            window.connect_close_request(move |_| {
                closing_loop.quit();
                glib::Propagation::Proceed
            });
            window.present();
            let _ = started.send(());
            // Keep ownership of the clipboard while the report is displayed,
            // including Wayland desktops without a clipboard manager.
            loop_.run();
            let _ = closed.send(());
        });
        if spawned.is_ok() && ready.recv_timeout(Duration::from_secs(3)).is_ok() {
            let _ = done.recv();
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn show_native(summary: &str, copy_failed: bool) -> Choice {
    let locale = i18n::crash_locale();
    let copy = msg!(CopyTrace).render_for(locale);
    let description = if copy_failed {
        format!("{summary}\n\n{}", msg!(CopyTraceFailed).render_for(locale))
    } else {
        summary.to_owned()
    };
    // No Floem window parent: RFD owns the modal dialog and main-thread dispatch.
    let result = rfd::MessageDialog::new()
        .set_title(msg!(CrashTitle).render_for(locale))
        .set_description(description)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            copy.clone(),
            msg!(Close).render_for(locale),
        ))
        .show();
    match result {
        rfd::MessageDialogResult::Custom(label) if label == copy => Choice::Copy,
        _ => Choice::Close,
    }
}

#[cfg(any(target_os = "macos", windows))]
fn copy_native(report: &str) -> Result<(), ()> {
    use copypasta::ClipboardProvider;

    copypasta::ClipboardContext::new()
        .and_then(|mut clipboard| clipboard.set_contents(report.to_owned()))
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            summary: "brief error".to_owned(),
            text: "complete report\nframe 1\nframe 2\n".to_owned(),
        }
    }

    #[test]
    fn capture_contains_metadata_and_forced_backtrace() {
        let report = Report::capture(Some(std::panic::Location::caller()));
        assert!(report.summary.contains("crash_dialog.rs:"));
        assert!(report.text.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.text.contains(std::env::consts::OS));
        assert!(report.text.contains(std::env::consts::ARCH));
        assert!(report.text.contains("unix="));
        assert!(report.text.contains("Thread:"));
        assert!(report.text.contains("Backtrace:\n"));
        assert!(!report.text.contains("disabled backtrace"));
        assert!(report.text.contains("crash_dialog::"));
        assert!(Report::capture(None).summary.contains("unknown location"));
    }

    #[test]
    fn capture_does_not_read_thread_name() {
        let secret = "protected body and password must stay private";
        // Thread names, like panic payloads, can contain application data.
        let captured = std::thread::Builder::new()
            .name(secret.to_owned())
            .spawn(|| Report::capture(Some(std::panic::Location::caller())))
            .unwrap()
            .join()
            .unwrap();
        assert!(!captured.summary.contains(secret));
        assert!(!captured.text.contains(secret));
        assert!(captured.text.contains("Panic payload omitted for privacy."));
    }

    #[test]
    fn copy_receives_complete_report_once() {
        let report = report();
        let mut copied = Vec::new();
        let mut shown = 0;
        present_report(
            &report,
            |summary, failed| {
                assert_eq!(summary, report.summary);
                assert!(!failed);
                shown += 1;
                Choice::Copy
            },
            |text| {
                copied.push(text.to_owned());
                Ok(())
            },
        );
        assert_eq!(shown, 1);
        assert_eq!(copied, [report.text]);
    }

    #[test]
    fn closing_does_not_touch_clipboard() {
        present_report(
            &report(),
            |_, _| Choice::Close,
            |_| panic!("closing must not copy"),
        );
    }

    #[test]
    fn failed_copy_can_be_retried_or_closed() {
        for retry in [false, true] {
            let mut shown = Vec::new();
            let mut copies = 0;
            present_report(
                &report(),
                |_, failed| {
                    shown.push(failed);
                    if !failed || retry {
                        Choice::Copy
                    } else {
                        Choice::Close
                    }
                },
                |_| {
                    copies += 1;
                    if copies == 1 { Err(()) } else { Ok(()) }
                },
            );
            assert_eq!(shown, [false, true]);
            assert_eq!(copies, if retry { 2 } else { 1 });
        }
    }

    #[test]
    fn concurrent_panics_admit_only_one_dialog() {
        let active = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let attempts = (0..8)
                .map(|_| scope.spawn(|| enter_once(&active)))
                .collect::<Vec<_>>();
            let entered = attempts
                .into_iter()
                .map(|attempt| usize::from(attempt.join().unwrap()))
                .sum::<usize>();
            assert_eq!(entered, 1);
        });
        assert!(!enter_once(&active));
    }

    #[test]
    fn log_failure_still_presents_the_report() {
        let mut presented = false;
        // Opening a directory as an append-only file must fail.
        let result = log_and_present(&std::env::temp_dir(), &report(), |_| {
            presented = true;
        });
        assert!(result.is_err());
        assert!(presented);
    }
}
