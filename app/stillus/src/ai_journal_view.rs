// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::ai_journal::{Filter, Summary};
use crate::*;
use stillus_ai::{AiProvider, journal::RequestStatus};

#[derive(Clone, PartialEq)]
struct Overview {
    operation: String,
    status: RequestStatus,
    duration: Option<u64>,
    model: Option<String>,
    error: Option<String>,
}

fn journal_list_height(count: usize) -> f64 {
    count.min(5) as f64 * 36.0
}

#[derive(Clone)]
struct JournalView {
    global: Rc<RefCell<GlobalApplication>>,
    open: RwSignal<bool>,
    rows: RwSignal<Vec<Summary>>,
    before: RwSignal<Option<String>>,
    has_more: RwSignal<bool>,
    provider: RwSignal<Option<AiProvider>>,
    status: RwSignal<Option<RequestStatus>>,
    selected: RwSignal<Option<String>>,
    detail: RwSignal<String>,
    overview: RwSignal<Option<Overview>>,
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
                view.has_more.set(page.has_more);
                if view.rows.get_untracked() != page.rows {
                    view.rows.set(page.rows);
                }
                let overview = page.detail.as_ref().map(|record| Overview {
                    operation: record.purpose.clone(),
                    status: record.status,
                    duration: record.duration_ms,
                    model: record.model.clone(),
                    error: record.error.clone(),
                });
                if view.overview.get_untracked() != overview {
                    view.overview.set(overview);
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
    page_at(global, open, palette, create_rw_signal(None))
}
pub(super) fn page_at(
    global: Rc<RefCell<GlobalApplication>>,
    open: RwSignal<bool>,
    palette: Palette,
    selected: RwSignal<Option<String>>,
) -> impl IntoView {
    let view = JournalView {
        global,
        open,
        rows: create_rw_signal(Vec::new()),
        before: create_rw_signal(None),
        has_more: create_rw_signal(false),
        provider: create_rw_signal(None),
        status: create_rw_signal(None),
        selected,
        detail: create_rw_signal(String::new()),
        overview: create_rw_signal(None),
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
    let overview = view.overview;
    let details_open = create_rw_signal(false);
    let provider = view.provider;
    let status = view.status;
    let before = view.before;
    let has_more = view.has_more;
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
        actions((
            toolbar_action_button(
                ButtonAction::Back,
                move || tr!(AiJournalBack),
                IconButtonTone::Secondary,
                palette,
                || true,
                move || open.set(false),
            ),
            toolbar_action_button(
                ButtonAction::Custom(ButtonAction::Delete.icon()),
                move || tr!(AiJournalClear),
                IconButtonTone::Danger,
                palette,
                move || !busy.get() && !rows.get().is_empty(),
                move || confirm.set(true),
            )
            .style(move |s| s.apply_if(rows.get().is_empty(), |s| s.hide())),
            toolbar_action_button(
                ButtonAction::Retry,
                move || tr!(AiJournalRetry),
                IconButtonTone::Secondary,
                palette,
                move || !busy.get(),
                move || retry.refresh(false, true),
            )
            .style(move |s| s.apply_if(!error.get(), |s| s.hide())),
        ))
        .style(move |s| s.flex_shrink(0.0)),
        page_title(i18n::Key::AiJournal, palette),
        page_description(i18n::Key::AiJournalHint, palette),
        actions((
            toolbar_action_button(
                ButtonAction::Custom(ButtonAction::Settings.icon()),
                move || {
                    provider
                        .get()
                        .map_or_else(|| tr!(AiJournalAllProviders), |p| p.name())
                },
                IconButtonTone::Secondary,
                palette,
                || true,
                move || {
                    let providers = stillus_ai::provider::ProviderRegistry::standard()
                        .providers()
                        .map(|p| p.id())
                        .collect::<Vec<_>>();
                    let current = provider.get_untracked();
                    let next = current
                        .as_ref()
                        .and_then(|id| providers.iter().position(|p| p == id))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    provider.set(providers.get(next).cloned());
                    before.set(None);
                },
            ),
            toolbar_action_button(
                ButtonAction::Custom(ButtonAction::Settings.icon()),
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
        ))
        .style(move |s| {
            s.flex_shrink(0.0).apply_if(
                rows.get().is_empty() && provider.get().is_none() && status.get().is_none(),
                |s| s.hide(),
            )
        }),
        label(move || tr!(AiJournalError))
            .style(move |s| s.color(palette.danger).apply_if(!error.get(), |s| s.hide())),
        v_stack((
            label(move || tr!(AiJournalConfirm)),
            actions((
                form_action_button(
                    ButtonAction::Delete,
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
                form_action_button(
                    ButtonAction::Cancel,
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
                    let selected_id = id.clone();
                    let timestamp = chrono::DateTime::from_timestamp_millis(row.started_ms as i64)
                        .map(|t| t.with_timezone(&chrono::Local).format("%Y/%m/%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    selectable_row(h_stack((
                        text(timestamp).style(move |s| s.width(180.0).flex_shrink(0.0).font_size(crate::ui::FONT_CAPTION).color(palette.ink2)),
                        label(move || row.provider.as_ref().map_or_else(|| tr!(AiJournalCorrupt), |p| p.name()))
                            .style(move |s| s.width(100.0).min_width(0.0).text_ellipsis().color(palette.ink)),
                        label(move || status_name(row.status)).style(move |s| s.min_width(0.0).flex_grow(1.0).text_ellipsis()
                            .color(if row.status == RequestStatus::Error { palette.danger } else { palette.ink })),
                    )).style(|s| rtl_row(s).width_full().min_width(0.0).items_center().gap(12.0)),
                        move || selected.set(Some(id.clone())))
                    .style(move |s| s.width_full().height(36.0).flex_shrink(0.0).padding_horiz(8.0).items_center()
                        .cursor(CursorStyle::Pointer).border_radius(4.0)
                        .background(if selected.get().as_ref() == Some(&selected_id) { palette.accent_soft } else { Color::TRANSPARENT })
                        .hover(|s| s.background(palette.accent_soft))
                        .focus_visible(|s| s.outline(1.0).outline_color(palette.accent)))
                },
            )
            .style(|s| s.flex_col().width_full()),
        )
        .style(move |s| {
            s.width_full()
                .height(journal_list_height(rows.get().len()))
                .min_height(0.0)
                .apply_if(rows.get().is_empty(), |s| s.hide())
        }),
        actions((
            toolbar_action_button(
                ButtonAction::Custom(ICON_ARROW_DOWN),
                move || tr!(AiJournalNewest),
                IconButtonTone::Secondary,
                palette,
                move || before.get().is_some(),
                move || before.set(None),
            ),
            toolbar_action_button(
                ButtonAction::Custom(ICON_ARROW_UP),
                move || tr!(AiJournalOlder),
                IconButtonTone::Secondary,
                palette,
                move || has_more.get(),
                move || before.set(rows.get_untracked().last().map(|r| r.id.clone())),
            ),
        ))
        .style(move |s| {
            s.flex_shrink(0.0)
                .apply_if(!has_more.get() && before.get().is_none(), |s| s.hide())
        }),
        v_stack((
            label(move || overview.get().map(|record| tr!(AiJournalSummary,
                "operation" => record.operation,
                "status" => status_name(record.status),
                "duration" => record.duration.map(|v| v.to_string()).unwrap_or_else(|| "—".to_owned()),
                "model" => record.model.unwrap_or_else(|| "—".to_owned())
            )).unwrap_or_default()).style(move |s| s.width_full().font_size(crate::ui::FONT_BODY).color(palette.ink)),
            label(move || overview.get().and_then(|v| v.error).unwrap_or_default())
                .style(move |s| s.width_full().font_size(crate::ui::FONT_BODY).color(palette.danger)
                    .apply_if(overview.get().is_none_or(|v| v.error.is_none()), |s| s.hide())),
            toolbar_action_button(ButtonAction::Custom(ICON_CHEVRON_DOWN), || tr!(AiJournalDetails),
                IconButtonTone::Secondary, palette, || true,
                move || details_open.update(|v| *v = !*v)),
        )).style(move |s| s.width_full().min_width(0.0).gap(8.0).padding(12.0).background(palette.paper)
            .border(1.0).border_color(palette.divider).border_radius(6.0)
            .apply_if(overview.get().is_none(), |s| s.hide())),
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
                |(i, line)| (*i, line.clone()),
                move |(_, line)| {
                    text(line).style(move |s| {
                        s.min_height(18.0)
                            .font_family(crate::ui::MONO_FONT_FAMILY.to_owned())
                            .font_size(crate::ui::FONT_CAPTION as f32)
                            .color(palette.ink)
                            .selectable(true)
                    })
                },
            )
            .style(|s| s.flex_col().min_width(0.0).padding_bottom(56.0)),
        )
        .style(move |s| {
            s.width_full()
                .min_width(0.0)
                .min_height(0.0)
                .flex_basis(0.0)
                .flex_grow(1.0)
                .apply_if(detail.get().is_empty() || !details_open.get(), |s| s.hide())
        }),
        actions((
            toolbar_action_button(
                ButtonAction::Custom(ButtonAction::Back.icon()),
                move || tr!(AiJournalPreviousPart),
                IconButtonTone::Secondary,
                palette,
                move || chunk.get() > 0,
                move || chunk.update(|n| *n = n.saturating_sub(1)),
            ),
            toolbar_action_button(
                ButtonAction::Custom(ICON_CHEVRON_RIGHT),
                move || tr!(AiJournalNextPart),
                IconButtonTone::Secondary,
                palette,
                move || detail.get().chars().count() > (chunk.get() + 1) * 8000,
                move || chunk.update(|n| *n += 1),
            ),
        ))
        .style(move |s| {
            s.flex_shrink(0.0)
                .apply_if(detail.get().chars().count() <= 8000 || !details_open.get(), |s| s.hide())
        }),
    ))
    .style(move |s| {
        rtl_column(s)
            .padding_horiz(SETTINGS_PAGE_INSET_PX)
            .padding_vert(30.0)
            .gap(10.0)
            .width_full()
            .min_width(0.0)
            .height_full()
            .min_height(0.0)
            .background(palette.canvas)
            .apply_if(!open.get(), |s| s.hide())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn journal_list_only_reserves_space_for_existing_rows() {
        for (count, expected) in [(0, 0.0), (1, 36.0), (3, 108.0), (50, 180.0)] {
            assert_eq!(super::journal_list_height(count), expected);
        }
    }
}
