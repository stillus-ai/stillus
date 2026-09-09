// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Native chat presentation. Drafts, requests and metadata go through Application.
use crate::*;
use floem::kurbo::Rect;

use application::{
    api::{Caller, Command as AppCommand, Query as AppQuery},
    chat::{Command, Query},
};
use stillus_chat::{Delivery, Role, RunStatus, ToolState};
use stillus_engine::CommonMetadataPatch;

fn dispatch(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    command: Command,
) -> Option<String> {
    let result = model
        .borrow_mut()
        .dispatch(Caller::Ui, AppCommand::Chat(command));
    revision.update(|v| *v += 1);
    match result {
        Ok(application::api::CommandResult::Accepted { operation, .. }) => Some(operation),
        Ok(_) => None,
        Err(error) => {
            model.borrow_mut().error = Some(UiText::Failure {
                details: format!("{error:?}"),
            });
            None
        }
    }
}
fn item(model: &Rc<RefCell<AppModel>>, id: &ItemId) -> Option<stillus_engine::ItemSummary> {
    model
        .borrow()
        .workspace
        .as_ref()?
        .non_document_items()
        .into_iter()
        .find(|i| i.engine_id == stillus_chat::engine_id() && &i.item_id == id)
}
fn patch(
    model: &Rc<RefCell<AppModel>>,
    id: &ItemId,
    revision: RwSignal<u64>,
    patch: CommonMetadataPatch,
    alias: Option<String>,
) {
    if let Some(item) = item(model, id) {
        dispatch(
            model,
            revision,
            Command::Metadata {
                id: id.to_string(),
                version: item.metadata_version,
                patch,
                alias,
            },
        );
    }
}
fn status(run: &RunStatus) -> String {
    match run {
        RunStatus::Queued => tr!(ChatQueued),
        RunStatus::Running => tr!(ChatRunning),
        RunStatus::Paused => tr!(ChatPaused),
        RunStatus::Completed => tr!(ChatCompleted),
        RunStatus::Failed => tr!(ChatFailed),
        RunStatus::Stopped | RunStatus::Interrupted => tr!(ChatInterrupted),
    }
}

pub(super) fn panel(
    model: Rc<RefCell<AppModel>>,
    id: ItemId,
    revision: RwSignal<u64>,
    settings: SettingsPageSignals,
    palette: Palette,
) -> AnyView {
    let _ = model.borrow_mut().query(
        Caller::Ui,
        AppQuery::Chat(Query::Read {
            id: id.to_string(),
            before: None,
            limit: 32,
        }),
    );
    let journal_open = create_rw_signal(false);
    let journal_selected = create_rw_signal(None::<String>);
    let draft = create_rw_signal(String::new());
    let loaded = create_rw_signal(false);
    let draft_version = create_rw_signal(String::new());
    let sending = create_rw_signal(false);
    let sent_from = create_rw_signal(None::<String>);
    let session = model.borrow().session_id();
    let composer_epoch = create_rw_signal(0u64);
    let follow = create_rw_signal(true);
    let content_height = create_rw_signal(0.0);
    let viewport_height = create_rw_signal(0.0);
    let scroll_to = create_rw_signal(None::<Point>);
    let history_width = create_rw_signal(1.0f64);
    let scroll_y = create_rw_signal(0.0);
    let paging = create_rw_signal(false);
    let anchor = create_rw_signal(None::<(String, f64)>);
    let row_bounds = Rc::new(RefCell::new(
        std::collections::BTreeMap::<String, Rect>::new(),
    ));
    let read_model = model.clone();
    let read_id = id.clone();
    let read_once = Rc::new(RefCell::new(None::<String>));
    let read_flag = read_once.clone();
    create_effect(move |_| {
        revision.get();
        let model = read_model.borrow();
        if let Some(view) = model.chat_view().filter(|v| v.id == read_id.as_str()) {
            if !loaded.get_untracked() {
                draft.set(view.draft.value.text.clone());
                draft_version.set(view.draft.revision.clone());
                loaded.set(true);
            }
            if sending.get_untracked()
                && view.run.as_ref().map(|r| r.value.id.clone()) != sent_from.get_untracked()
                && view.draft.value.text.is_empty()
            {
                draft.set(String::new());
                composer_epoch.update(|v| *v += 1);
                sending.set(false);
                anchor.set(None);
                follow.set(true);
            } else if sending.get_untracked() && !model.chat_running(&read_id) {
                sending.set(false);
            }
            if !model.chat_draft_pending(&read_id) {
                draft_version.set(view.draft.revision.clone());
            }
            if view.run.as_ref().is_some_and(|r| r.value.unread)
                && !model.chat_running(&read_id)
                && follow.get()
                && !settings.open.get()
                && !journal_open.get()
                && view
                    .run
                    .as_ref()
                    .is_some_and(|r| read_flag.borrow().as_ref() != Some(&r.value.id))
            {
                *read_flag.borrow_mut() = view.run.as_ref().map(|r| r.value.id.clone());
                let model = read_model.clone();
                let id = read_id.clone();
                exec_after(Duration::from_millis(10), move |_| {
                    if model.borrow().session_id() == session {
                        dispatch(&model, revision, Command::Seen { id: id.to_string() });
                    }
                });
            }
        }
    });
    let title_model = model.clone();
    let title_id = id.clone();
    let title_click_model = model.clone();
    let title_click_id = id.clone();
    let rename = ToolbarEditBar {
        open: create_rw_signal(false),
        value: create_rw_signal(String::new()),
        label: i18n::Key::NewTitle,
        placeholder: i18n::Key::ChatNew,
    };
    let categories = ToolbarEditBar {
        open: create_rw_signal(false),
        value: create_rw_signal(String::new()),
        label: i18n::Key::CategoriesPlaceholder,
        placeholder: i18n::Key::CategoriesExample,
    };
    let toolbar_model = model.clone();
    let toolbar_state = model.clone();
    let toolbar_id = id.clone();
    let toolbar_state_id = id.clone();
    let toolbar = dyn_container(
        move || {
            revision.get();
            item(&toolbar_state, &toolbar_state_id)
                .map(|i| i.metadata.deleted)
                .unwrap_or(false)
        },
        move |deleted| {
            if rename.open.try_get_untracked().is_none() {
                return empty().into_any();
            }
            let declared = toolbar_model
                .borrow()
                .workspace
                .as_ref()
                .map(|w| w.engine_toolbar_actions(&stillus_chat::engine_id()))
                .unwrap_or_default();
            let controls = visible_toolbar_actions(&declared, deleted)
                .into_iter()
                .map(|action| {
                    let model = toolbar_model.clone();
                    let id = toolbar_id.clone();
                    let state_model = model.clone();
                    let state_id = id.clone();
                    toolbar_control(
                        action,
                        ToolbarSubject::Chat,
                        palette,
                        move || {
                            revision.get();
                            match action {
                                ToolbarAction::Rename => rename.open.get(),
                                ToolbarAction::Categories => categories.open.get(),
                                ToolbarAction::Pin => {
                                    item(&state_model, &state_id).is_some_and(|i| i.metadata.pinned)
                                }
                                ToolbarAction::Favorite => item(&state_model, &state_id)
                                    .is_some_and(|i| i.metadata.favorited),
                                _ => false,
                            }
                        },
                        move || {
                            let Some(current) = item(&model, &id) else {
                                return;
                            };
                            match action {
                                ToolbarAction::Rename => {
                                    rename.value.set(current.metadata.title);
                                    rename.open.update(|v| *v = !*v);
                                    categories.open.set(false)
                                }
                                ToolbarAction::Categories => {
                                    categories.value.set(current.metadata.categories.join(", "));
                                    categories.open.update(|v| *v = !*v);
                                    rename.open.set(false)
                                }
                                ToolbarAction::Pin => patch(
                                    &model,
                                    &id,
                                    revision,
                                    CommonMetadataPatch {
                                        pinned: Some(!current.metadata.pinned),
                                        ..Default::default()
                                    },
                                    None,
                                ),
                                ToolbarAction::Favorite => patch(
                                    &model,
                                    &id,
                                    revision,
                                    CommonMetadataPatch {
                                        favorited: Some(!current.metadata.favorited),
                                        ..Default::default()
                                    },
                                    None,
                                ),
                                ToolbarAction::Delete | ToolbarAction::Restore => {
                                    patch(
                                        &model,
                                        &id,
                                        revision,
                                        CommonMetadataPatch {
                                            deleted: Some(!deleted),
                                            ..Default::default()
                                        },
                                        None,
                                    );
                                }
                                _ => {}
                            }
                        },
                    )
                    .into_any()
                })
                .collect::<Vec<_>>();
            h_stack_from_iter(controls)
                .style(|s| s.items_center().gap(TOOLBAR_ACTION_GAP_PX))
                .into_any()
        },
    );
    let journal_model = model.clone();
    let journal_button = toolbar_action_button(
        ButtonAction::Custom(ICON_JOURNAL),
        || tr!(AiJournal),
        IconButtonTone::Secondary,
        palette,
        || true,
        move || {
            journal_selected.set(
                journal_model
                    .borrow()
                    .chat_view()
                    .and_then(|v| v.run.as_ref())
                    .and_then(|r| r.value.pending_request.clone()),
            );
            journal_open.set(true);
        },
    );
    let rename_model = model.clone();
    let rename_id = id.clone();
    let rename_bar = toolbar_edit_bar(rename, palette, move || {
        patch(
            &rename_model,
            &rename_id,
            revision,
            CommonMetadataPatch {
                title: Some(rename.value.get_untracked()),
                ..Default::default()
            },
            None,
        );
        rename.open.set(false);
    });
    let categories_model = model.clone();
    let categories_id = id.clone();
    let categories_bar = toolbar_edit_bar(categories, palette, move || {
        patch(
            &categories_model,
            &categories_id,
            revision,
            CommonMetadataPatch {
                categories: Some(
                    categories
                        .value
                        .get_untracked()
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                ),
                ..Default::default()
            },
            None,
        );
        categories.open.set(false);
    });
    let rows_model = model.clone();
    let row_model = model.clone();
    let rows_id = id.clone();
    let bounds_for_rows = row_bounds.clone();
    let bounds_for_views = row_bounds.clone();
    let rows = dyn_stack(
        move || {
            revision.get();
            let entries = rows_model
                .borrow()
                .chat_view()
                .filter(|v| v.id == rows_id.as_str())
                .map(|v| v.history.entries.clone())
                .unwrap_or_default();
            bounds_for_rows
                .borrow_mut()
                .retain(|id, _| entries.iter().any(|entry| &entry.id == id));
            entries
        },
        |e| {
            let signature = e
                .message
                .as_ref()
                .map(|m| format!("{}{:?}{:?}", m.value.text, m.value.delivery, m.value.tool));
            (e.id.clone(), signature)
        },
        move |entry| {
            let id = entry.id.clone();
            let bounds = bounds_for_views.clone();
            message_view(entry, row_model.clone(), revision, history_width, palette).on_resize(
                move |rect| {
                    bounds.borrow_mut().insert(id.clone(), rect);
                    if let Some((target, offset)) = anchor.try_get_untracked().flatten() {
                        if target == id {
                            scroll_to.set(Some(Point::new(0.0, (rect.y0 + offset).max(0.0))));
                        }
                    }
                },
            )
        },
    )
    .style(move |s| {
        s.width(history_width.get())
            .min_width(0.0)
            .flex_col()
            .gap(18.0)
    });
    let earlier_model = model.clone();
    let earlier_id = id.clone();
    let earlier_state = model.clone();
    let earlier_state_id = id.clone();
    let earlier = enabled_icon_button(
        ICON_ARROW_UP,
        || tr!(ChatLoadEarlier),
        IconButtonTone::Secondary,
        palette,
        move || {
            revision.get();
            !paging.get()
                && earlier_state
                    .borrow()
                    .chat_view()
                    .filter(|v| v.id == earlier_state_id.as_str())
                    .is_some_and(|v| v.history.next.is_some())
        },
        move || {
            let before = earlier_model
                .borrow()
                .chat_view()
                .and_then(|v| v.history.next.clone());
            let y = scroll_y.get_untracked();
            anchor.set(
                row_bounds
                    .borrow()
                    .iter()
                    .filter(|(_, rect)| rect.y1 > y)
                    .min_by(|a, b| a.1.y0.total_cmp(&b.1.y0))
                    .map(|(id, rect)| (id.clone(), y - rect.y0)),
            );
            follow.set(false);
            load_history(
                earlier_model.clone(),
                earlier_id.to_string(),
                before,
                paging,
                revision,
            );
        },
    );
    let newest_state = model.clone();
    let newest_model = model.clone();
    let newest_id = id.clone();
    let newest = enabled_icon_button(
        ICON_ARROW_DOWN,
        || tr!(AiJournalNewest),
        IconButtonTone::Secondary,
        palette,
        move || {
            revision.get();
            !paging.get()
                && (!follow.get()
                    || newest_state
                        .borrow()
                        .chat_view()
                        .is_some_and(|v| v.before.is_some()))
        },
        move || {
            anchor.set(None);
            follow.set(true);
            scroll_to.set(Some(Point::new(0.0, content_height.get_untracked())));
            load_history(
                newest_model.clone(),
                newest_id.to_string(),
                None,
                paging,
                revision,
            );
        },
    );
    let header = ui::content_header(
        ICON_CHAT,
        move || {
            revision.get();
            item(&title_model, &title_id)
                .map(|item| item.metadata.title)
                .unwrap_or_else(|| tr!(ChatNew))
        },
        h_stack((journal_button, earlier, newest, toolbar))
            .style(|style| style.items_center().gap(TOOLBAR_ACTION_GAP_PX)),
        Some(Rc::new(move || {
            if let Some(item) = item(&title_click_model, &title_click_id) {
                rename.value.set(item.metadata.title);
                categories.open.set(false);
                rename.open.set(true);
            }
        })),
        palette,
    );
    let empty_state = model.clone();
    let empty_hint = label(|| tr!(ChatEmpty)).style(move |s| {
        revision.get();
        s.width_full()
            .min_width(0.0)
            .padding(20.0)
            .color(palette.muted)
            .apply_if(
                empty_state
                    .borrow()
                    .chat_view()
                    .is_some_and(|v| !v.history.entries.is_empty()),
                |s| s.hide(),
            )
    });
    let history_content = v_stack((empty_hint, rows))
        .style(move |s| s.width(history_width.get()).min_width(0.0).gap(12.0))
        .on_resize(move |r| {
            content_height.set(r.height());
            // Scroll clamps its viewport before this callback when a page shrinks.
            // Reconcile against the new height, not the preceding page's height.
            let at_bottom =
                scroll_y.get_untracked() + viewport_height.get_untracked() >= r.height() - 40.0;
            // Paging owns the viewport until explicit pointer navigation or Newest.
            // A transient clamp during relayout must not re-enable following and
            // overwrite the saved visible-message anchor.
            if anchor.get_untracked().is_none() && (follow.get_untracked() || at_bottom) {
                follow.set(true);
                scroll_to.set(Some(Point::new(
                    0.0,
                    (r.height() - viewport_height.get_untracked()).max(0.0),
                )));
            }
        });
    let history_scrollbar_visible = create_rw_signal(false);
    let history_scrollbar_generation = create_rw_signal(0_u64);
    let history_scroll_origin = create_rw_signal(None::<Point>);
    let history = scroll(history_content)
        .on_resize(move |rect| {
            // Leave the history scrollbar beside text, including long links.
            history_width.set((rect.width() - 12.0).max(1.0));
            viewport_height.set(rect.height());
        })
        .on_event_cont(EventListener::PointerWheel, move |_| anchor.set(None))
        .on_event_cont(EventListener::PointerDown, move |_| anchor.set(None))
        .on_scroll(move |viewport| {
            let origin = viewport.origin();
            if history_scroll_origin
                .get_untracked()
                .is_some_and(|previous| previous != origin)
            {
                show_scrollbar_temporarily(history_scrollbar_visible, history_scrollbar_generation);
            }
            history_scroll_origin.set(Some(origin));
            scroll_y.set(viewport.y0);
            viewport_height.set(viewport.height());
            follow.set(
                anchor.get_untracked().is_none()
                    && viewport.y1 >= content_height.get_untracked() - 40.0,
            );
        })
        .scroll_to(move || scroll_to.get())
        .scroll_style(move |s| {
            s.handle_thickness(8.0)
                .vertical_track_inset(2.0)
                .handle_background(Color::rgba8(
                    palette.muted.r,
                    palette.muted.g,
                    palette.muted.b,
                    128,
                ))
                .hide_bars(!history_scrollbar_visible.get())
        })
        .style(|s| {
            s.width_full()
                .min_width(0.0)
                .min_height(0.0)
                .flex_basis(0.0)
                .flex_grow(1.0)
        });
    let alias_state = model.clone();
    let alias_model = model.clone();
    let alias_id = id.clone();
    let alias_state_id = id.clone();
    let alias = dyn_container(
        move || {
            revision.get();
            let m = alias_state.borrow();
            let names = m
                .global
                .as_ref()
                .map(|g| g.borrow().ai().aliases.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let value = m
                .chat_view()
                .filter(|v| v.id == alias_state_id.as_str())
                .map(|v| v.metadata.value.alias.clone())
                .unwrap_or_else(|| "default".into());
            (names, value)
        },
        move |(names, value)| {
            let selected = create_rw_signal(Some(value));
            let model = alias_model.clone();
            let id = alias_id.clone();
            select(
                selected,
                names,
                |value| value.unwrap_or_default(),
                move |alias| {
                    patch(
                        &model,
                        &id,
                        revision,
                        CommonMetadataPatch::default(),
                        Some(alias),
                    );
                },
                || true,
                palette,
            )
            .into_any()
        },
    )
    .style(|s| s.width(240.0).min_width(0.0).flex_shrink(1.0));
    let alias = h_stack((
        label(|| format!("{}:", tr!(AiModelLabel))).style(move |s| {
            s.font_size(crate::ui::FONT_BODY as f32)
                .color(palette.ink)
                .flex_shrink(0.0)
        }),
        alias,
    ))
    .style(|s| s.items_center().gap(8.0).min_width(0.0).flex_shrink(1.0));
    let submit_model = model.clone();
    let submit_id = id.clone();
    let submit: Rc<dyn Fn()> = Rc::new(move || {
        if sending.get_untracked()
            || draft.get_untracked().trim().is_empty()
            || submit_model.borrow().chat_running(&submit_id)
        {
            return;
        }
        if submit_model.borrow().session_id() != session {
            return;
        }
        if submit_model
            .borrow()
            .global
            .as_ref()
            .is_none_or(|g| g.borrow().ai().connection.is_none())
        {
            settings.section.set(SettingsSection::Ai);
            settings.open.set(true);
            return;
        }
        let version = submit_model
            .borrow()
            .chat_view()
            .filter(|v| v.id == submit_id.as_str())
            .map(|v| v.draft.revision.clone())
            .unwrap_or_default();
        sent_from.set(
            submit_model
                .borrow()
                .chat_view()
                .and_then(|v| v.run.as_ref().map(|r| r.value.id.clone())),
        );
        sending.set(true);
        dispatch(
            &submit_model,
            revision,
            Command::Send {
                id: submit_id.to_string(),
                version,
            },
        );
    });
    let composer_submit = submit.clone();
    let composer_model = model.clone();
    let composer_id = id.clone();
    let composer = dyn_container(
        move || {
            (
                loaded.get(),
                composer_epoch.get(),
                settings.open.get() || journal_open.get(),
            )
        },
        move |(ready, _, hidden)| {
            if !ready || hidden {
                return empty().into_any();
            }
            let submit = composer_submit.clone();
            let model = composer_model.clone();
            let id = composer_id.clone();
            TextArea::new(draft, palette)
                .placeholder(i18n::Key::ChatPlaceholder)
                .height(116.0)
                .enabled(move || !sending.get())
                .visible(move || !settings.open.get() && !journal_open.get())
                .on_submit(move || submit())
                .build(move |value| {
                    if loaded.try_get_untracked().is_none()
                        || settings.open.get_untracked()
                        || journal_open.get_untracked()
                        || model.borrow().session_id() != session
                    {
                        return;
                    }
                    draft.set(value.clone());
                    dispatch(
                        &model,
                        revision,
                        Command::Compose {
                            id: id.to_string(),
                            version: draft_version.get_untracked(),
                            text: value,
                        },
                    );
                })
        },
    );
    let send_model = model.clone();
    let send_id = id.clone();
    let send = form_action_button(
        ButtonAction::Send,
        || tr!(ChatSend),
        IconButtonTone::Primary,
        palette,
        move || {
            revision.get();
            loaded.get()
                && !sending.get()
                && !draft.get().trim().is_empty()
                && !send_model.borrow().chat_running(&send_id)
        },
        move || submit(),
    );
    let stop_model = model.clone();
    let stop_id = id.clone();
    let stop_state = model.clone();
    let stop_state_id = id.clone();
    let stop = form_action_button(
        ButtonAction::Stop,
        || tr!(ChatStop),
        IconButtonTone::Secondary,
        palette,
        move || {
            revision.get();
            stop_state.borrow().chat_running(&stop_state_id)
        },
        move || {
            dispatch(
                &stop_model,
                revision,
                Command::Stop {
                    id: stop_id.to_string(),
                },
            );
        },
    );
    let continue_model = model.clone();
    let continue_id = id.clone();
    let continue_state = model.clone();
    let continue_state_id = id.clone();
    let continue_button = form_action_button(
        ButtonAction::Custom(ButtonAction::Send.icon()),
        || tr!(ChatContinue),
        IconButtonTone::Secondary,
        palette,
        move || {
            revision.get();
            continue_state
                .borrow()
                .chat_items()
                .iter()
                .find(|i| i.item.item_id == continue_state_id)
                .and_then(|i| i.run.as_ref())
                .is_some_and(|r| r.value.status == RunStatus::Paused)
        },
        move || {
            dispatch(
                &continue_model,
                revision,
                Command::Continue {
                    id: continue_id.to_string(),
                },
            );
        },
    );
    let connect = form_action_button(
        ButtonAction::Custom(ButtonAction::Settings.icon()),
        || tr!(ChatConnect),
        IconButtonTone::Secondary,
        palette,
        || true,
        move || {
            settings.section.set(SettingsSection::Ai);
            settings.open.set(true);
        },
    );
    let connected_model = model.clone();
    let connected_id = id.clone();
    let readiness = floem::reactive::create_memo(move |_| {
        revision.get();
        connected_model
            .borrow()
            .chat_generation_readiness(&connected_id)
    });
    let connected = floem::reactive::create_memo(move |_| {
        readiness.get() == application::chat::GenerationReadiness::Ready
    });
    let unavailable = label(move || match readiness.get() {
        application::chat::GenerationReadiness::Unsupported => tr!(ChatUnsupported),
        application::chat::GenerationReadiness::Unavailable => tr!(AiUnavailable),
        _ => String::new(),
    })
    .style(move |s| {
        s.color(palette.muted)
            .font_size(crate::ui::FONT_CAPTION as f32)
            .apply_if(
                matches!(
                    readiness.get(),
                    application::chat::GenerationReadiness::Ready
                        | application::chat::GenerationReadiness::Disconnected
                ),
                |s| s.hide(),
            )
    });
    let controls = h_stack((
        alias,
        empty().style(|s| s.flex_grow(1.0)),
        actions((continue_button, stop, send)).style(|s| s.min_width(0.0)),
    ))
    .style(move |s| {
        s.width_full()
            .min_width(0.0)
            .flex_shrink(0.0)
            .flex_wrap(floem::style::FlexWrap::Wrap)
            .gap(8.0)
            .items_center()
            .apply_if(!connected.get(), |s| s.hide())
    });
    let connect = connect.style(move |s| s.apply_if(connected.get(), |s| s.hide()));
    let status_model = model.clone();
    let status_id = id.clone();
    let status_label = label(move || {
        revision.get();
        let mut m = status_model.borrow_mut();
        match m.query(
            Caller::Ui,
            AppQuery::Chat(Query::State {
                id: status_id.to_string(),
            }),
        ) {
            Ok(application::api::QueryResult::Chat(application::chat::Output::State {
                run: Some(run),
            })) => format!(
                "{}{}",
                status(&run.status),
                run.error
                    .map(|e| format!(
                        " · {}",
                        if e.contains("Journal") {
                            tr!(AiJournalError)
                        } else if e.contains("RequiresUserInteraction") {
                            tr!(AiUnavailable)
                        } else if e.contains("Conflict") {
                            tr!(Conflict)
                        } else {
                            tr!(ChatFailed)
                        }
                    ))
                    .unwrap_or_default()
            ),
            _ => {
                if m.chat_view().is_some_and(|v| !v.diagnostics.is_empty()) {
                    tr!(AiJournalCorrupt)
                } else {
                    String::new()
                }
            }
        }
    })
    .style(move |s| {
        s.color(palette.muted)
            .font_size(crate::ui::FONT_CAPTION as f32)
            .flex_shrink(0.0)
    });
    let body = v_stack((
        rename_bar,
        categories_bar,
        history,
        status_label,
        unavailable,
        composer,
        controls,
        connect,
    ))
    .style(move |s| {
        s.width_full()
            .min_width(0.0)
            .height_full()
            .min_height(0.0)
            .flex_basis(0.0)
            .flex_grow(1.0)
            .padding(20.0)
            .gap(12.0)
            .background(palette.paper)
    });
    let body = v_stack((header, body)).style(move |s| {
        s.width_full()
            .height_full()
            .min_width(0.0)
            .min_height(0.0)
            .apply_if(journal_open.get(), |s| s.hide())
    });
    let journal = model
        .borrow()
        .global
        .as_ref()
        .map(|g| {
            crate::ai_journal_view::page_at(g.clone(), journal_open, palette, journal_selected)
                .into_any()
        })
        .unwrap_or_else(|| empty().into_any());
    stack((body, journal))
        .style(|s| s.width_full().min_width(0.0).height_full().min_height(0.0))
        .into_any()
}

fn message_view(
    entry: stillus_chat::HistoryEntry,
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    history_width: RwSignal<f64>,
    palette: Palette,
) -> AnyView {
    // Floem may drain a queued DynStack update after its chat panel scope has
    // been disposed. Do not construct rich text or effects for that old panel.
    if history_width.try_get_untracked().is_none() {
        return empty().into_any();
    }
    let version = entry
        .message
        .as_ref()
        .map(|m| m.revision.clone())
        .unwrap_or_default();
    let Some(message) = entry.message.map(|m| m.value) else {
        return text(format!(
            "{}: {}",
            tr!(ChatFailed),
            entry.diagnostic.unwrap_or_default()
        ))
        .into_any();
    };
    if let Some(tool) = message.tool {
        let open = create_rw_signal(false);
        let unknown = tool.state == ToolState::Unknown;
        let confirm = create_rw_signal(false);
        let ack_model = model.clone();
        let message_id = message.id.clone();
        let state = match tool.state {
            ToolState::Prepared => tr!(ChatRunning),
            ToolState::Completed => tr!(ChatCompleted),
            ToolState::Failed => tr!(ChatFailed),
            ToolState::Unknown => tr!(ChatUnknown),
        };
        let title = format!("{} · {} · {state}", tr!(ChatTool), tool.name);
        let detail = serde_json::to_string_pretty(
            &serde_json::json!({"arguments":tool.arguments,"result":tool.result}),
        )
        .unwrap_or_default();
        let acknowledge = form_action_button(
            ButtonAction::Custom(ButtonAction::Save.icon()),
            move || {
                if confirm.get() {
                    tr!(Confirm)
                } else {
                    tr!(ChatAcknowledge)
                }
            },
            IconButtonTone::Secondary,
            palette,
            move || unknown,
            move || {
                if !confirm.get_untracked() {
                    confirm.set(true);
                    return;
                }
                let id = ack_model.borrow().chat_view().map(|v| v.id.clone());
                if let Some(id) = id {
                    dispatch(
                        &ack_model,
                        revision,
                        Command::Acknowledge {
                            id,
                            message: message_id.clone(),
                            version: version.clone(),
                        },
                    );
                }
            },
        )
        .style(move |s| s.apply_if(!unknown, |s| s.hide()));
        return v_stack((
            acknowledge,
            content_button(
                ICON_CHEVRON_DOWN,
                text(title).style(move |s| {
                    s.font_size(crate::ui::FONT_CAPTION as f32)
                        .line_height(1.5)
                        .color(palette.muted)
                }),
                move || open.update(|v| *v = !*v),
            )
            .style(|s| s.min_width(0.0).width_full()),
            wrapped_text(
                detail,
                move || (history_width.get() - 20.0).max(1.0),
                palette.muted,
                crate::ui::FONT_CAPTION as f32,
            )
            .style(move |s| s.apply_if(!open.get(), |s| s.hide())),
        ))
        .style(|s| s.width_full().padding(10.0).gap(8.0))
        .into_any();
    }
    let role = match message.role {
        Role::User => tr!(ChatUser),
        _ => tr!(ChatAssistant),
    };
    let role = if message.delivery == Delivery::Interrupted {
        format!("{role} · {}", tr!(ChatInterrupted))
    } else {
        role
    };
    let user = message.role == Role::User;
    let message_width = floem::reactive::create_memo(move |_| {
        if user {
            (history_width.get() * 0.75 - 24.0).max(1.0)
        } else {
            history_width.get().max(1.0)
        }
    });
    let rendered = if user {
        wrapped_text(
            message.text.clone(),
            move || message_width.get(),
            palette.ink,
            crate::ui::FONT_BODY as f32,
        )
    } else {
        markdown_blocks(&message.text, message_width, palette)
    };
    let link_views = message_links(&message.text)
        .into_iter()
        .map(|(url, title)| {
            let model = model.clone();
            let hint = url.clone();
            anchored_tooltip(
                selectable_row(
                    wrapped_text(
                        title,
                        move || message_width.get(),
                        palette.accent,
                        crate::ui::FONT_BODY as f32,
                    ),
                    move || {
                        if let Err(e) = open_rss_original(&url) {
                            model.borrow_mut().error = Some(e.to_string().into());
                        }
                    },
                )
                .style(move |s| {
                    s.width_full()
                        .min_width(0.0)
                        .cursor(CursorStyle::Pointer)
                        .focus_visible(|s| s.background(palette.accent_soft))
                }),
                Rc::new(move || hint.clone()),
                palette,
            )
        })
        .collect::<Vec<_>>();
    let content = message.text.clone();
    let copy = toolbar_action_button(
        ButtonAction::Copy,
        || tr!(Copy),
        IconButtonTone::Secondary,
        palette,
        || true,
        move || {
            let _ = Clipboard::set_contents(content.clone());
        },
    );
    let bubble = v_stack((
        h_stack((
            text(role).style(move |s| {
                s.color(palette.muted)
                    .font_size(crate::ui::FONT_CAPTION as f32)
            }),
            empty().style(|s| s.flex_grow(1.0)),
            copy,
        )),
        rendered,
        v_stack_from_iter(link_views).style(|s| s.width_full().gap(4.0)),
    ))
    .style(move |s| {
        s.width(message_width.get() + if user { 24.0 } else { 0.0 })
            .min_width(0.0)
            .gap(6.0)
            .apply_if(user, |s| {
                s.padding(12.0)
                    .border_radius(10.0)
                    .background(palette.canvas)
            })
    });
    h_stack((bubble,))
        .style(move |s| {
            s.width_full()
                .min_width(0.0)
                .apply_if(user, |s| s.justify_end())
        })
        .into_any()
}

fn message_links(source: &str) -> Vec<(String, String)> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let mut links = Vec::new();
    let mut active = None::<(String, String)>;
    let mut seen = HashSet::new();
    for event in pulldown_cmark::Parser::new(source) {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                active = rss_card::article_url(&dest_url).map(|url| (url, String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, label)) = &mut active {
                    label.push_str(&text);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, label)) = active.take()
                    && seen.insert(url.clone())
                {
                    let label = if label.trim().is_empty() || label == url {
                        url::Url::parse(&url)
                            .ok()
                            .and_then(|url| url.host_str().map(str::to_owned))
                            .unwrap_or_else(|| url.clone())
                    } else {
                        label
                    };
                    links.push((url, label));
                }
            }
            _ => {}
        }
    }
    links
}

fn markdown_blocks(source: &str, width: floem::reactive::Memo<f64>, palette: Palette) -> AnyView {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let mut blocks = Vec::new();
    let mut cursor = 0;
    let mut code_start = None;
    let mut code = String::new();
    for (event, range) in pulldown_cmark::Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code_start = Some(range.start);
                code.clear();
            }
            Event::Text(value) if code_start.is_some() => code.push_str(&value),
            Event::End(TagEnd::CodeBlock) => {
                let start = code_start.take().expect("code block");
                if start > cursor {
                    blocks.push(markdown_prose(&source[cursor..start], width, palette));
                }
                let content = code.clone();
                let hovered = create_rw_signal(false);
                let focused = create_rw_signal(false);
                let measured = create_rw_signal(0.0_f64);
                let viewport_width =
                    floem::reactive::create_memo(move |_| (width.get() - 64.0).max(1.0));
                let overflow = floem::reactive::create_memo(move |_| {
                    measured.get() > viewport_width.get() + 0.5
                });
                let scroll_origin = create_rw_signal(Point::ZERO);
                let requested_scroll = create_rw_signal(None::<Point>);
                let copy = toolbar_action_button(
                    ButtonAction::Copy,
                    || tr!(Copy),
                    IconButtonTone::Secondary,
                    palette,
                    || true,
                    move || {
                        let _ = Clipboard::set_contents(content.clone());
                    },
                );
                blocks.push(
                    h_stack((
                        scroll(
                            text(code.clone())
                                .style(move |s| {
                                    s.text_clip()
                                        .padding_bottom(if overflow.get() { 16.0 } else { 0.0 })
                                        .font_family(crate::ui::MONO_FONT_FAMILY.to_owned())
                                        .font_size(crate::ui::FONT_BODY as f32)
                                        .color(palette.ink)
                                })
                                .on_resize(move |rect| measured.set(rect.width())),
                        )
                        .on_scroll(move |viewport| scroll_origin.set(viewport.origin()))
                        .scroll_to(move || requested_scroll.get())
                        .keyboard_navigable()
                        .on_event_cont(EventListener::FocusGained, move |_| focused.set(true))
                        .on_event_cont(EventListener::FocusLost, move |_| focused.set(false))
                        .on_event(EventListener::KeyDown, move |event| {
                            let floem::event::Event::KeyDown(key) = event else {
                                return EventPropagation::Continue;
                            };
                            let maximum = (measured.get_untracked()
                                - viewport_width.get_untracked())
                            .max(0.0);
                            let x = match key.key.logical_key {
                                Key::Named(NamedKey::ArrowLeft) => {
                                    (scroll_origin.get_untracked().x - 60.0).max(0.0)
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    (scroll_origin.get_untracked().x + 60.0).min(maximum)
                                }
                                Key::Named(NamedKey::Home) => 0.0,
                                Key::Named(NamedKey::End) => maximum,
                                _ => return EventPropagation::Continue,
                            };
                            requested_scroll.set(Some(Point::new(x, 0.0)));
                            EventPropagation::Stop
                        })
                        .scroll_style(move |s| {
                            s.handle_thickness(8.0)
                                .handle_background(palette.muted)
                                .hide_bars(!overflow.get() || !(hovered.get() || focused.get()))
                        })
                        .style(move |s| {
                            s.width(viewport_width.get())
                                .min_width(0.0)
                                .focus_visible(|s| s.background(palette.accent_soft))
                        }),
                        copy,
                    ))
                    .on_event_cont(EventListener::PointerEnter, move |_| hovered.set(true))
                    .on_event_cont(EventListener::PointerLeave, move |_| hovered.set(false))
                    .style(move |s| {
                        s.width_full()
                            .padding(12.0)
                            .gap(8.0)
                            .items_start()
                            .background(palette.canvas)
                            .border_radius(6.0)
                    })
                    .into_any(),
                );
                cursor = range.end;
            }
            _ => {}
        }
    }
    if cursor < source.len() {
        blocks.push(markdown_prose(&source[cursor..], width, palette));
    }
    v_stack_from_iter(blocks)
        .style(|s| s.width_full().gap(8.0))
        .into_any()
}
fn markdown_prose(source: &str, width: floem::reactive::Memo<f64>, palette: Palette) -> AnyView {
    let layout = rss_card::markdown(source).layout(palette.ink);
    floem::views::rich_text(move || {
        let mut layout = layout.clone();
        layout.set_wrap(floem::text::Wrap::WordOrGlyph);
        layout.set_size(width.get().max(1.0) as f32, f32::MAX);
        layout
    })
    .style(move |s| s.width(width.get()).min_width(0.0))
    .into_any()
}

fn wrapped_text(
    text: String,
    width: impl Fn() -> f64 + Copy + 'static,
    color: Color,
    size: f32,
) -> AnyView {
    floem::views::rich_text(move || wrapped_text_layout(&text, width(), color, size))
        .style(move |s| s.width(width()).min_width(0.0))
        .into_any()
}

fn wrapped_text_layout(text: &str, width: f64, color: Color, size: f32) -> floem::text::TextLayout {
    let mut layout = floem::text::TextLayout::new();
    let family = [floem::text::FamilyOwned::Name(
        crate::ui::UI_FONT_FAMILY.to_owned(),
    )];
    let attrs = floem::text::Attrs::new()
        .family(&family)
        .font_size(size)
        .color(color)
        .line_height(floem::text::LineHeightValue::Normal(1.45));
    layout.set_text(text, floem::text::AttrsList::new(attrs));
    layout.set_wrap(floem::text::Wrap::WordOrGlyph);
    layout.set_size(width.max(1.0) as f32, f32::MAX);
    layout
}

fn load_history(
    model: Rc<RefCell<AppModel>>,
    id: String,
    before: Option<String>,
    busy: RwSignal<bool>,
    revision: RwSignal<u64>,
) {
    if busy.get_untracked() {
        return;
    }
    let result = model.borrow_mut().query(
        Caller::Ui,
        AppQuery::Chat(Query::Read {
            id,
            before,
            limit: 32,
        }),
    );
    if let Ok(application::api::QueryResult::Pending { operation }) = result {
        busy.set(true);
        poll_history(model, operation, busy, revision);
    }
}
fn poll_history(
    model: Rc<RefCell<AppModel>>,
    operation: String,
    busy: RwSignal<bool>,
    revision: RwSignal<u64>,
) {
    exec_after(Duration::from_millis(50), move |_| {
        if busy.try_get_untracked().is_none() {
            return;
        }
        let result = model
            .borrow_mut()
            .query(Caller::Ui, AppQuery::OperationProgress(operation.clone()));
        if matches!(
            result,
            Ok(application::api::QueryResult::Operation {
                status: application::actions::OperationStatus::Pending
                    | application::actions::OperationStatus::Running,
                ..
            })
        ) {
            poll_history(model, operation, busy, revision);
        } else {
            busy.set(false);
            revision.update(|r| *r += 1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use floem::reactive::{Scope, as_child_of_current_scope, with_scope};
    use stillus_chat::{HistoryEntry, Message, ToolCall, Versioned};

    fn entry(role: Role) -> HistoryEntry {
        HistoryEntry {
            id: "message/1".to_owned(),
            message: Some(Versioned {
                revision: "revision/1".to_owned(),
                value: Message {
                    id: "message/1".to_owned(),
                    run: "run/1".to_owned(),
                    role,
                    text: "**Reply**\n\n```rust\nlet n = 1;\n```\n\n[Link](https://example.com)"
                        .to_owned(),
                    delivery: Delivery::Partial,
                    created_ms: 0,
                    tool: None,
                    provider_state: None,
                    request_id: None,
                },
            }),
            diagnostic: None,
        }
    }

    #[test]
    fn message_links_preserve_labels_deduplicate_and_reject_unsafe_urls() {
        let links = message_links(
            "[Read **more**](https://example.com/a) [again](https://example.com/a) [bad](file:///tmp/private) <https://example.com/long>",
        );
        assert_eq!(
            links,
            vec![
                ("https://example.com/a".to_owned(), "Read more".to_owned()),
                (
                    "https://example.com/long".to_owned(),
                    "example.com".to_owned()
                )
            ]
        );
    }

    #[test]
    fn long_links_wrap_inside_the_available_message_width() {
        let link = format!("https://example.test/{}", "longsegment".repeat(30));
        for width in [180.0, 360.0, 720.0] {
            let layout = wrapped_text_layout(&link, width, Color::BLACK, 14.0);
            assert!(layout.layout_runs().count() > 1);
            assert!(layout.size().width <= width + 1.0);
        }
    }

    #[test]
    fn queued_messages_are_ignored_after_chat_panel_disposal() {
        let root = Scope::new();
        let revision = root.create_rw_signal(0_u64);
        let model = Rc::new(RefCell::new(AppModel::unloaded()));
        let mut tool = entry(Role::Assistant);
        tool.message.as_mut().unwrap().value.tool = Some(ToolCall {
            id: "tool/1".to_owned(),
            name: "read_note".to_owned(),
            arguments: serde_json::json!({}),
            state: ToolState::Completed,
            result: Some(serde_json::json!({"text": "result"})),
        });
        for entry in [entry(Role::User), entry(Role::Assistant), tool] {
            let panel_scope = root.create_child();
            let width = panel_scope.create_rw_signal(640.0);
            let row_model = model.clone();
            // DynStack captures the panel scope and calls this factory later,
            // while draining queued view updates.
            let build = with_scope(panel_scope, || {
                as_child_of_current_scope(move |entry| {
                    message_view(entry, row_model.clone(), revision, width, Palette::new())
                })
            });
            let (mounted, _) = build(entry.clone());
            assert!(!mounted.id().children().is_empty());
            width.set(320.0);
            panel_scope.dispose();
            assert!(width.try_get_untracked().is_none());
            assert_eq!(revision.get_untracked(), 0);

            let (late, late_scope) = build(entry);
            assert_eq!(
                floem::View::debug_name(late.as_ref()),
                std::any::type_name::<floem::views::Empty>()
            );
            late_scope.dispose();
        }
        root.dispose();
    }
}
