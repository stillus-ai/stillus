// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::ai_journal::{Filter, PAGE_SIZE, Summary};
use crate::*;
use stillus_ai::{AiProvider, journal::RequestStatus};

#[derive(Clone)]
struct JournalView {
    global: Rc<RefCell<GlobalApplication>>,
    open: RwSignal<bool>,
    rows: RwSignal<Vec<Summary>>,
    before: RwSignal<Option<String>>,
    provider: RwSignal<Option<AiProvider>>,
    status: RwSignal<Option<RequestStatus>>,
    selected: RwSignal<Option<String>>,
    detail: RwSignal<String>,
    error: RwSignal<bool>,
    busy: RwSignal<bool>,
    confirm: RwSignal<bool>,
    generation: Rc<Cell<u64>>,
    watch_generation: Rc<Cell<u64>>,
    pending: Rc<Cell<(bool, bool)>>,
}

impl JournalView {
    fn refresh(&self, clear: bool, retry: bool) {
        if self.busy.get_untracked() {
            let (pending_clear, pending_retry) = self.pending.get();
            self.pending
                .set((pending_clear || clear, pending_retry || retry));
            return;
        }
        self.busy.set(true);
        let generation = self.generation.get();
        let before = self.before.get_untracked();
        let filter = Filter {
            provider: self.provider.get_untracked(),
            status: self.status.get_untracked(),
        };
        let selected = self.selected.get_untracked();
        let result = self
            .global
            .borrow_mut()
            .start_journal(application::global::JournalRequest {
                before,
                filter,
                selected,
                clear,
                retry,
            });
        match result {
            Ok(id) => poll(self.clone(), id, generation),
            Err(_) => {
                self.busy.set(false);
                self.error.set(true);
            }
        }
    }

    fn drain_pending(&self) {
        let (clear, retry) = self.pending.replace((false, false));
        if clear || retry {
            self.refresh(clear, retry);
        }
    }
}

fn poll(view: JournalView, id: u64, generation: u64) {
    exec_after(Duration::from_millis(50), move |_| {
        if view.busy.try_get_untracked().is_none() {
            return;
        }
        let result = view.global.borrow().journal_result(id);
        let Some(result) = result else {
            poll(view, id, generation);
            return;
        };
        view.busy.set(false);
        if generation != view.generation.get() {
            let (clear, retry) = view.pending.replace((false, false));
            view.refresh(clear, retry);
            return;
        }
        match result {
            Ok(page) => {
                if view.rows.get_untracked() != page.rows {
                    view.rows.set(page.rows);
                }
                let detail = page
                    .detail
                    .map(|record| serde_json::to_string_pretty(&record))
                    .transpose();
                match detail {
                    Ok(detail) => {
                        let detail = detail.unwrap_or_default();
                        if view.detail.get_untracked() != detail {
                            view.detail.set(detail);
                        }
                        view.error.set(page.blocked);
                    }
                    Err(_) => view.error.set(true),
                }
            }
            Err(_) => view.error.set(true),
        }
        view.drain_pending();
    });
}

fn watch(view: JournalView, generation: u64) {
    exec_after(Duration::from_secs(1), move |_| {
        if view.watch_generation.get() != generation
            || !view.open.try_get_untracked().unwrap_or(false)
        {
            return;
        }
        if !view.confirm.get_untracked() {
            view.refresh(false, false);
        }
        watch(view, generation);
    });
}

fn status_name(status: RequestStatus) -> String {
    match status {
        RequestStatus::Pending => tr!(AiJournalPending),
        RequestStatus::Success => tr!(AiJournalSuccess),
        RequestStatus::Error => tr!(AiJournalFailed),
        RequestStatus::Unknown => tr!(AiJournalUnknown),
    }
}

pub(super) fn page(
    global: Rc<RefCell<GlobalApplication>>,
    open: RwSignal<bool>,
    palette: Palette,
) -> impl IntoView {
    let view = JournalView {
        global,
        open,
        rows: create_rw_signal(Vec::new()),
        before: create_rw_signal(None),
        provider: create_rw_signal(None),
        status: create_rw_signal(None),
        selected: create_rw_signal(None),
        detail: create_rw_signal(String::new()),
        error: create_rw_signal(false),
        busy: create_rw_signal(false),
        confirm: create_rw_signal(false),
        generation: Rc::new(Cell::new(0)),
        watch_generation: Rc::new(Cell::new(0)),
        pending: Rc::new(Cell::new((false, false))),
    };
    let lifecycle = view.clone();
    create_effect(move |_| {
        let visible = open.get();
        lifecycle
            .watch_generation
            .set(lifecycle.watch_generation.get() + 1);
        if visible {
            lifecycle.generation.set(lifecycle.generation.get() + 1);
            lifecycle.refresh(false, false);
            watch(lifecycle.clone(), lifecycle.watch_generation.get());
        }
    });
    let filter_view = view.clone();
    create_effect(move |_| {
        filter_view.provider.get();
        filter_view.status.get();
        filter_view.before.get();
        filter_view.selected.get();
        filter_view.generation.set(filter_view.generation.get() + 1);
        if open.get_untracked() {
            filter_view.refresh(false, false);
        }
    });
    let rows = view.rows;
    let selected = view.selected;
    let detail = view.detail;
    let provider = view.provider;
    let status = view.status;
    let before = view.before;
    let confirm = view.confirm;
    let busy = view.busy;
    let error = view.error;
    let clear = view.clone();
    let retry = view.clone();
    let chunk = create_rw_signal(0usize);
    create_effect(move |_| {
        selected.get();
        chunk.set(0);
    });
    v_stack((
        crate::ai_settings::actions((
            action_button(
                move || tr!(AiJournalBack),
                IconButtonTone::Secondary,
                palette,
                || true,
                move || open.set(false),
            ),
            action_button(
                move || tr!(AiJournalClear),
                IconButtonTone::Danger,
                palette,
                || true,
                move || confirm.set(true),
            ),
            action_button(
                move || tr!(AiJournalRetry),
                IconButtonTone::Secondary,
                palette,
                || true,
                move || retry.refresh(false, true),
            ),
        )),
        crate::ai_settings::page_title(i18n::Key::AiJournal, palette),
        crate::ai_settings::page_description(i18n::Key::AiJournalHint, palette),
        crate::ai_settings::actions((
            action_button(
                move || {
                    provider
                        .get()
                        .map_or_else(|| tr!(AiJournalAllProviders), |p| p.name().into())
                },
                IconButtonTone::Secondary,
                palette,
                || true,
                move || {
                    provider.set(match provider.get_untracked() {
                        None => Some(AiProvider::OpenAi),
                        Some(AiProvider::OpenAi) => Some(AiProvider::Anthropic),
                        _ => None,
                    });
                    before.set(None);
                },
            ),
            action_button(
                move || {
                    status
                        .get()
                        .map_or_else(|| tr!(AiJournalAllStatuses), status_name)
                },
                IconButtonTone::Secondary,
                palette,
                || true,
                move || {
                    status.set(match status.get_untracked() {
                        None => Some(RequestStatus::Success),
                        Some(RequestStatus::Success) => Some(RequestStatus::Error),
                        Some(RequestStatus::Error) => Some(RequestStatus::Pending),
                        Some(RequestStatus::Pending) => Some(RequestStatus::Unknown),
                        _ => None,
                    });
                    before.set(None);
                },
            ),
        )),
        label(move || tr!(AiJournalError))
            .style(move |s| s.color(palette.danger).apply_if(!error.get(), |s| s.hide())),
        v_stack((
            label(move || tr!(AiJournalConfirm)),
            crate::ai_settings::actions((
                action_button(
                    move || tr!(AiJournalClear),
                    IconButtonTone::Danger,
                    palette,
                    move || !busy.get(),
                    move || {
                        confirm.set(false);
                        selected.set(None);
                        clear.refresh(true, false);
                    },
                ),
                action_button(
                    move || tr!(Cancel),
                    IconButtonTone::Secondary,
                    palette,
                    || true,
                    move || confirm.set(false),
                ),
            )),
        ))
        .style(move |s| s.apply_if(!confirm.get(), |s| s.hide())),
        label(move || tr!(AiJournalEmpty))
            .style(move |s| s.apply_if(!rows.get().is_empty(), |s| s.hide())),
        scroll(
            dyn_stack(
                move || rows.get(),
                |row| row.id.clone(),
                move |row| {
                    let id = row.id.clone();
                    label(move || {
                        let timestamp =
                            chrono::DateTime::from_timestamp_millis(row.started_ms as i64)
                                .map(|t| {
                                    t.with_timezone(&chrono::Local)
                                        .format("%Y/%m/%d %H:%M:%S")
                                        .to_string()
                                })
                                .unwrap_or_default();
                        format!(
                            "{}   {}   {}",
                            timestamp,
                            row.provider
                                .map_or_else(|| tr!(AiJournalCorrupt), |p| p.name().into()),
                            status_name(row.status)
                        )
                    })
                    .on_click_stop(move |_| selected.set(Some(id.clone())))
                    .style(move |s| {
                        s.width_full()
                            .padding(8.0)
                            .cursor(CursorStyle::Pointer)
                            .hover(|s| s.background(palette.accent_soft))
                    })
                },
            )
            .style(|s| s.flex_col().width_full()),
        )
        .style(|s| s.width_full().height(180.0)),
        crate::ai_settings::actions((
            action_button(
                move || tr!(AiJournalNewest),
                IconButtonTone::Secondary,
                palette,
                move || before.get().is_some(),
                move || before.set(None),
            ),
            action_button(
                move || tr!(AiJournalOlder),
                IconButtonTone::Secondary,
                palette,
                move || rows.get().len() == PAGE_SIZE,
                move || before.set(rows.get_untracked().last().map(|r| r.id.clone())),
            ),
        )),
        scroll(
            dyn_stack(
                move || {
                    detail
                        .get()
                        .chars()
                        .skip(chunk.get() * 8000)
                        .take(8000)
                        .collect::<String>()
                        .lines()
                        .enumerate()
                        .map(|(i, line)| (i, line.to_owned()))
                        .collect::<Vec<_>>()
                },
                |(i, _)| *i,
                move |(_, line)| {
                    text(line).style(move |s| {
                        s.min_height(18.0)
                            .font_family("monospace".to_owned())
                            .font_size(12.0)
                            .color(palette.ink)
                            .selectable(true)
                    })
                },
            )
            .style(|s| s.flex_col().min_width(0.0)),
        )
        .style(|s| s.width_full().min_height(100.0).flex_grow(1.0)),
        crate::ai_settings::actions((
            action_button(
                move || tr!(AiJournalPreviousPart),
                IconButtonTone::Secondary,
                palette,
                move || chunk.get() > 0,
                move || chunk.update(|n| *n = n.saturating_sub(1)),
            ),
            action_button(
                move || tr!(AiJournalNextPart),
                IconButtonTone::Secondary,
                palette,
                move || detail.get().chars().count() > (chunk.get() + 1) * 8000,
                move || chunk.update(|n| *n += 1),
            ),
        )),
    ))
    .style(move |s| {
        rtl_column(s)
            .padding(30.0)
            .gap(10.0)
            .width_full()
            .height_full()
            .background(palette.canvas)
            .apply_if(!open.get(), |s| s.hide())
    })
}
