// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Sidebar actions retain the path/item and workspace session captured on opening.
use crate::*;
use stillus_engine::CommonMetadataPatch;

fn changed(model: &Rc<RefCell<AppModel>>, revision: RwSignal<u64>) {
    model.borrow_mut().sync_effects();
    revision.update(|value| *value = value.saturating_add(1));
    schedule_autosave(model.clone(), revision);
}

fn entry(
    icon: &'static str,
    key: i18n::Key,
    enabled: bool,
    action: impl Fn() + 'static,
) -> MenuEntry {
    MenuEntry::action(icon, move || key.to_string(), move || enabled, action)
}

fn edit_card(
    title: i18n::Key,
    initial: String,
    open: RwSignal<bool>,
    palette: Palette,
    submit: impl Fn(&str) -> Result<(), UiText> + 'static,
) -> AnyView {
    let value = create_rw_signal(initial);
    let error = create_rw_signal(None::<UiText>);
    let submit: Rc<dyn Fn()> = Rc::new(move || match submit(&value.get_untracked()) {
        Ok(()) => open.set(false),
        Err(message) => error.set(Some(message)),
    });
    let key_submit = submit.clone();
    let input = localized_input::LocalizedInput::new(value, title)
        .on_escape(move || open.set(false))
        .on_event(EventListener::KeyDown, move |event| {
            if matches!(event, Event::KeyDown(key) if key.key.logical_key == Key::Named(NamedKey::Enter)) {
                key_submit();
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .style(move |style| form_field_style(style, palette, error.get().is_some()).width_full());
    input.id().request_focus();
    v_stack((
        text(title).style(move |s| s.font_size(crate::ui::FONT_CARD as f32).color(palette.ink)),
        input,
        label(move || {
            error
                .get()
                .as_ref()
                .map(i18n::user_error_text)
                .unwrap_or_default()
        })
        .style(move |s| {
            s.font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.danger)
        }),
        h_stack((
            dialog_button(
                ButtonAction::Cancel,
                msg!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || open.set(false),
            ),
            form_action_button(
                ButtonAction::Save,
                || tr!(Save),
                IconButtonTone::Primary,
                palette,
                || true,
                move || submit(),
            ),
        ))
        .style(|s| s.width_full().justify_end().gap(8.0)),
    ))
    .style(move |s| {
        dialog_card_style(s, palette, 360.0, 12.0)
            .width_full()
            .gap(10.0)
    })
    .style(|s| s.width_full().min_width(0.0))
    .into_any()
}

fn failure(model: &Rc<RefCell<AppModel>>) -> UiText {
    model
        .borrow()
        .error
        .clone()
        .unwrap_or_else(|| msg!(ResolveSaveFirst).into())
}

#[derive(Clone, Copy, PartialEq)]
enum NoteForm {
    Rename,
    Tags,
    Protection,
    Delete,
}

pub(super) fn note(
    trigger: impl IntoView + 'static,
    note: stillus_core::NoteSummary,
    model: Rc<RefCell<AppModel>>,
    sidebar_state: RwSignal<SidebarState>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let session = model.borrow().session_id();
    let form = create_rw_signal(NoteForm::Rename);
    let open = create_rw_signal(false);
    let waiting = create_rw_signal(false);
    let unlock_seen = create_rw_signal(false);
    let deletion = create_rw_signal(None::<stillus_core::PermanentNoteDeletion>);
    let path = note.path.clone();
    let wait_model = model.clone();
    let wait_path = path.clone();
    let wait_security = model.borrow().security_ui.clone();
    create_effect(move |_| {
        revision.get();
        if !waiting.get() {
            return;
        }
        let selected = {
            let model = wait_model.borrow();
            model.session_id() == session
                && model.workspace.as_ref().is_some_and(|w| {
                    w.selected_note()
                        .and_then(|index| w.notes().get(index))
                        .is_some_and(|n| n.path == wait_path)
                })
        };
        let has_document = wait_model
            .borrow()
            .workspace
            .as_ref()
            .is_some_and(|w| w.document().is_some());
        let unlocking = wait_security
            .as_ref()
            .is_some_and(|security| security.dialog.get().is_some());
        if unlocking {
            unlock_seen.set(true);
        }
        if selected && has_document && !unlocking {
            waiting.set(false);
            open.set(true);
        } else if (!selected && wait_model.borrow().pending_note_path.as_ref() != Some(&wait_path))
            || (selected
                && !has_document
                && unlock_seen.get_untracked()
                && !unlocking
                && !wait_model.borrow().secure_worker_active)
        {
            // Another navigation, a rejected request, or a workspace switch
            // cancels the follow-up instead of reopening it on a later visit.
            waiting.set(false);
        }
    });
    let request_model = model.clone();
    let request_path = path.clone();
    let request: Rc<dyn Fn(NoteForm)> = Rc::new(move |kind| {
        if request_model.borrow().session_id() != session {
            return;
        }
        form.set(kind);
        unlock_seen.set(false);
        if kind == NoteForm::Delete {
            let prepared = request_model
                .borrow()
                .prepare_catalog_note_deletion(&request_path);
            match prepared {
                Ok(token) => {
                    deletion.set(Some(token));
                    open.set(true);
                }
                Err(error) => request_model.borrow_mut().error = Some(error),
            }
        } else {
            let index = request_model
                .borrow()
                .workspace
                .as_ref()
                .and_then(|w| w.notes().iter().position(|n| n.path == request_path));
            if let Some(index) = index {
                waiting.set(true);
                let already_open =
                    request_model.borrow().workspace.as_ref().is_some_and(|w| {
                        w.selected_note() == Some(index) && w.document().is_some()
                    });
                if !already_open {
                    request_model.borrow_mut().open_note(index);
                }
            }
        }
        changed(&request_model, revision);
    });
    let entries_model = model.clone();
    let entries_path = path.clone();
    let trigger = context_menu_view(
        trigger.into_view().style(|s| s.width_full().min_width(0.0)),
        palette,
        move || {
            let model_ref = entries_model.borrow();
            if model_ref.session_id() != session {
                return vec![];
            }
            let current = model_ref
                .workspace
                .as_ref()
                .and_then(|w| w.notes().iter().find(|n| n.path == entries_path))
                .cloned();
            let selected = model_ref.workspace.as_ref().is_some_and(|w| {
                w.selected_note()
                    .and_then(|i| w.notes().get(i))
                    .is_some_and(|n| n.path == entries_path)
            });
            let unlocked = selected
                && model_ref
                    .workspace
                    .as_ref()
                    .is_some_and(|w| w.document().is_some());
            let busy = model_ref.deferred_note_action_pending()
                || model_ref.secure_worker_active
                || (!selected
                    && (model_ref.save_worker_active
                        || model_ref
                            .workspace
                            .as_ref()
                            .is_some_and(|w| w.actions_busy())));
            drop(model_ref);
            let Some(note) = current else {
                return vec![];
            };
            let ready = note.availability.is_ready() && !busy;
            let protection_key = if note.protection == NoteProtection::Plain {
                i18n::Key::ProtectNote
            } else if unlocked {
                i18n::Key::LockNote
            } else {
                i18n::Key::UnlockNote
            };
            let mut entries = Vec::new();
            let action_model = entries_model.clone();
            let action_path = entries_path.clone();
            entries.push(entry(ICON_NOTE, i18n::Key::Open, true, move || {
                if action_model.borrow().session_id() != session {
                    return;
                }
                let index = action_model
                    .borrow()
                    .workspace
                    .as_ref()
                    .and_then(|w| w.notes().iter().position(|n| n.path == action_path));
                if let Some(index) = index {
                    action_model.borrow_mut().open_note(index);
                    changed(&action_model, revision);
                }
            }));
            if note.deleted {
                let action_model = entries_model.clone();
                let action_path = entries_path.clone();
                entries.push(entry(
                    ICON_RETRY,
                    i18n::Key::RestoreNote,
                    ready,
                    move || {
                        if action_model.borrow().session_id() != session {
                            return;
                        }
                        action_model
                            .borrow_mut()
                            .set_catalog_note_deleted(&action_path, false);
                        changed(&action_model, revision);
                    },
                ));
                let request = request.clone();
                entries.push(
                    entry(ICON_TRASH, i18n::Key::DeletePermanently, ready, move || {
                        request(NoteForm::Delete)
                    })
                    .danger(true),
                );
                return entries;
            }
            let action_model = entries_model.clone();
            let action_path = entries_path.clone();
            entries.push(entry(
                ICON_PIN,
                if note.pinned {
                    i18n::Key::UnpinNote
                } else {
                    i18n::Key::PinNote
                },
                ready,
                move || {
                    if action_model.borrow().session_id() != session {
                        return;
                    }
                    action_model
                        .borrow_mut()
                        .set_catalog_note_pinned(&action_path, !note.pinned);
                    changed(&action_model, revision);
                },
            ));
            let action_model = entries_model.clone();
            let action_path = entries_path.clone();
            entries.push(entry(
                ICON_STAR,
                if note.favorited {
                    i18n::Key::RemoveFavorite
                } else {
                    i18n::Key::AddFavorite
                },
                ready,
                move || {
                    if action_model.borrow().session_id() != session {
                        return;
                    }
                    action_model
                        .borrow_mut()
                        .set_catalog_note_favorited(&action_path, !note.favorited);
                    changed(&action_model, revision);
                },
            ));
            for (kind, icon, key) in [
                (NoteForm::Tags, ICON_TAG, i18n::Key::ManageTags),
                (NoteForm::Protection, ICON_LOCK, protection_key),
                (NoteForm::Rename, ICON_RENAME, i18n::Key::RenameNote),
            ] {
                let request = request.clone();
                entries.push(entry(icon, key, ready, move || request(kind)));
            }
            let action_model = entries_model.clone();
            let action_path = entries_path.clone();
            entries.push(
                entry(ICON_TRASH, i18n::Key::TrashNote, ready, move || {
                    if action_model.borrow().session_id() != session {
                        return;
                    }
                    action_model
                        .borrow_mut()
                        .set_catalog_note_deleted(&action_path, true);
                    changed(&action_model, revision);
                })
                .danger(true),
            );
            entries
        },
    )
    .style(|s| s.width_full().min_width(0.0));
    let anchor_id = trigger.into_view();
    let trigger_id = anchor_id.id();
    anchored_popover(anchor_id, open, 360.0, 4.0, true, move || {
        if model.borrow().session_id() != session {
            return empty().into_any();
        }
        match form.get_untracked() {
            NoteForm::Tags => {
                let signals = TagPopoverSignals {
                    open,
                    target_path: create_rw_signal(Some(path.clone())),
                    query: create_rw_signal(String::new()),
                    highlighted: create_rw_signal(None),
                    hovered_tag: create_rw_signal(None),
                    trigger_pointer_down: create_rw_signal(false),
                };
                tag_popover_card(
                    model.clone(),
                    revision,
                    sidebar_state,
                    signals,
                    trigger_id,
                    palette,
                )
                .into_any()
            }
            NoteForm::Rename => {
                let initial = model
                    .borrow()
                    .workspace
                    .as_ref()
                    .and_then(|w| w.notes().iter().find(|n| n.path == path))
                    .map(|n| n.title.clone())
                    .unwrap_or_default();
                let model = model.clone();
                let path = path.clone();
                edit_card(
                    i18n::Key::RenameNote,
                    initial,
                    open,
                    palette,
                    move |title| {
                        let result = {
                            let mut model = model.borrow_mut();
                            if model.session_id() != session {
                                return Err(msg!(SelectionNotOpen).into());
                            }
                            model.edit_catalog_note_title(&path, title)
                        };
                        changed(&model, revision);
                        result.map(|_| ())
                    },
                )
            }
            NoteForm::Protection => {
                let security = model.borrow().security_ui.clone();
                match protection_action_state(&model.borrow()) {
                    ProtectionActionState::Protect => {
                        if let Some(security) = security {
                            let dialog = model
                                .borrow()
                                .workspace
                                .as_ref()
                                .map(protection_password_dialog);
                            if let Some(dialog) = dialog {
                                open.set(false);
                                security.open(dialog);
                            }
                        }
                        empty().into_any()
                    }
                    ProtectionActionState::Unlock { note_index } => {
                        if let Some(security) = security {
                            open.set(false);
                            security.open(PasswordDialogKind::Unlock { note_index });
                        }
                        empty().into_any()
                    }
                    _ => protection_popover(model.clone(), revision, open, palette).into_any(),
                }
            }
            NoteForm::Delete => {
                let action_model = model.clone();
                confirmation(
                    i18n::Key::DeletePermanently,
                    i18n::Key::DeletePermanentlyHint,
                    open,
                    palette,
                    move || {
                        if action_model.borrow().session_id() != session {
                            return Err(msg!(SelectionNotOpen).into());
                        }
                        let Some(token) = deletion.get_untracked() else {
                            return Err(msg!(SelectionNotOpen).into());
                        };
                        let success = action_model
                            .borrow_mut()
                            .delete_catalog_note_permanently(token);
                        changed(&action_model, revision);
                        if success {
                            Ok(())
                        } else {
                            Err(failure(&action_model))
                        }
                    },
                )
            }
        }
    })
    .style(|s| s.width_full().min_width(0.0))
    .into_any()
}

fn confirmation(
    title: i18n::Key,
    hint: i18n::Key,
    open: RwSignal<bool>,
    palette: Palette,
    action: impl Fn() -> Result<(), UiText> + 'static,
) -> AnyView {
    let error = create_rw_signal(None::<UiText>);
    let cancel = dialog_button(
        ButtonAction::Cancel,
        msg!(Cancel),
        IconButtonTone::Secondary,
        palette,
        move || open.set(false),
    );
    cancel.id().request_focus();
    v_stack((
        text(title).style(move |s| s.font_size(crate::ui::FONT_CARD as f32).color(palette.ink)),
        text(hint).style(move |s| {
            s.font_size(crate::ui::FONT_BODY as f32)
                .color(palette.muted)
        }),
        label(move || {
            error
                .get()
                .as_ref()
                .map(i18n::user_error_text)
                .unwrap_or_default()
        })
        .style(move |s| {
            s.color(palette.danger)
                .font_size(crate::ui::FONT_CAPTION as f32)
        }),
        h_stack((
            cancel,
            form_action_button(
                ButtonAction::Delete,
                move || title.to_string(),
                IconButtonTone::Danger,
                palette,
                || true,
                move || match action() {
                    Ok(()) => open.set(false),
                    Err(message) => error.set(Some(message)),
                },
            ),
        ))
        .style(|s| s.width_full().justify_end().gap(8.0)),
    ))
    .style(move |s| {
        dialog_card_style(s, palette, 360.0, 12.0)
            .width_full()
            .gap(10.0)
    })
    .style(|s| s.width_full().min_width(0.0))
    .into_any()
}

#[derive(Clone, Copy)]
enum EngineForm {
    Rename,
    Categories,
    Filters,
}

fn engine_patch(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    session: Option<application::api::SessionId>,
    summary: &stillus_engine::ItemSummary,
    patch: CommonMetadataPatch,
) -> Result<(), UiText> {
    if model.borrow().session_id() != session {
        return Err(msg!(SelectionNotOpen).into());
    }
    let command = if summary.engine_id == stillus_chat::engine_id() {
        application::api::Command::Chat(application::chat::Command::Metadata {
            id: summary.item_id.to_string(),
            version: summary.metadata_version.clone(),
            patch,
            alias: None,
        })
    } else {
        application::api::Command::Rss(rss_service::Addressed::Metadata {
            id: summary.item_id.clone(),
            version: summary
                .metadata_version
                .parse()
                .map_err(|_| UiText::from(msg!(SelectionNotOpen)))?,
            patch,
        })
    };
    let result = model
        .borrow_mut()
        .dispatch(application::api::Caller::Ui, command)
        .map_err(|error| UiText::Failure {
            details: format!("{error:?}"),
        });
    if let Err(error) = &result {
        model.borrow_mut().error = Some(error.clone());
    }
    changed(model, revision);
    result.map(|_| ())
}

pub(super) fn engine(
    trigger: impl IntoView + 'static,
    summary: stillus_engine::ItemSummary,
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let session = model.borrow().session_id();
    let open = create_rw_signal(false);
    let form = create_rw_signal(EngineForm::Rename);
    let captured = create_rw_signal(summary.clone());
    let entries_model = model.clone();
    let target_engine = summary.engine_id.clone();
    let target_id = summary.item_id.clone();
    let trigger = context_menu_view(
        trigger.into_view().style(|s| s.width_full().min_width(0.0)),
        palette,
        move || {
            if entries_model.borrow().session_id() != session {
                return vec![];
            }
            let summary = entries_model.borrow().workspace.as_ref().and_then(|w| {
                w.non_document_items()
                    .into_iter()
                    .find(|s| s.engine_id == target_engine && s.item_id == target_id)
            });
            let Some(summary) = summary else {
                return vec![];
            };
            captured.set(summary.clone());
            let chat = summary.engine_id == stillus_chat::engine_id();
            let ready = matches!(summary.availability, stillus_core::ItemAvailability::Ready);
            let mut entries = Vec::new();
            if !chat {
                let model = entries_model.clone();
                let id = summary.item_id.clone();
                entries.push(entry(
                    ICON_RETRY,
                    i18n::Key::RefreshFeed,
                    ready,
                    move || {
                        if model.borrow().session_id() != session {
                            return;
                        }
                        if model.borrow_mut().start_rss_refresh(id.clone()) {
                            schedule_rss_poll(model.clone(), revision);
                        }
                        changed(&model, revision);
                    },
                ));
                entries.push(entry(
                    ButtonAction::Settings.icon(),
                    i18n::Key::RssFilters,
                    ready,
                    move || {
                        form.set(EngineForm::Filters);
                        open.set(true);
                    },
                ));
            }
            entries.push(entry(
                ICON_RENAME,
                if chat {
                    i18n::Key::ChatRename
                } else {
                    i18n::Key::RenameFeed
                },
                ready,
                move || {
                    form.set(EngineForm::Rename);
                    open.set(true);
                },
            ));
            entries.push(entry(
                ICON_TAG,
                if chat {
                    i18n::Key::ChatCategories
                } else {
                    i18n::Key::EditFeedCategories
                },
                ready,
                move || {
                    form.set(EngineForm::Categories);
                    open.set(true);
                },
            ));
            if chat {
                let model = entries_model.clone();
                let target = summary.clone();
                entries.push(entry(
                    ICON_PIN,
                    if summary.metadata.pinned {
                        i18n::Key::ChatUnpin
                    } else {
                        i18n::Key::ChatPin
                    },
                    ready,
                    move || {
                        let _ = engine_patch(
                            &model,
                            revision,
                            session,
                            &target,
                            CommonMetadataPatch {
                                pinned: Some(!target.metadata.pinned),
                                ..Default::default()
                            },
                        );
                    },
                ));
            }
            let model = entries_model.clone();
            let deleted = summary.metadata.deleted;
            entries.push(
                entry(
                    if deleted { ICON_RETRY } else { ICON_TRASH },
                    if chat {
                        if deleted {
                            i18n::Key::ChatRestore
                        } else {
                            i18n::Key::ChatTrash
                        }
                    } else if deleted {
                        i18n::Key::RestoreFeed
                    } else {
                        i18n::Key::TrashFeed
                    },
                    ready,
                    move || {
                        let _ = engine_patch(
                            &model,
                            revision,
                            session,
                            &summary,
                            CommonMetadataPatch {
                                deleted: Some(!deleted),
                                ..Default::default()
                            },
                        );
                    },
                )
                .danger(!deleted),
            );
            entries
        },
    )
    .style(|s| s.width_full().min_width(0.0));
    anchored_popover(trigger, open, 480.0, 4.0, true, move || {
        let summary = captured.get_untracked();
        match form.get_untracked() {
            EngineForm::Filters => {
                rss_filters::form(model.clone(), summary.item_id, revision, open, palette)
            }
            kind => {
                let categories = matches!(kind, EngineForm::Categories);
                let initial = if categories {
                    summary.metadata.categories.join(", ")
                } else {
                    summary.metadata.title.clone()
                };
                let model = model.clone();
                edit_card(
                    if categories {
                        i18n::Key::CategoriesPlaceholder
                    } else {
                        i18n::Key::NewTitle
                    },
                    initial,
                    open,
                    palette,
                    move |value| {
                        let patch = if categories {
                            CommonMetadataPatch {
                                categories: Some(
                                    value
                                        .split(',')
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_owned)
                                        .collect(),
                                ),
                                ..Default::default()
                            }
                        } else {
                            CommonMetadataPatch {
                                title: Some(value.trim().to_owned()),
                                ..Default::default()
                            }
                        };
                        engine_patch(&model, revision, session, &summary, patch)
                    },
                )
            }
        }
    })
    .style(|s| s.width_full().min_width(0.0))
    .into_any()
}

#[derive(Clone, Copy)]
enum CategoryForm {
    Rename,
    Delete,
    Sort,
}

pub(super) fn category(
    trigger: impl IntoView + 'static,
    filter: SidebarFilter,
    model: Rc<RefCell<AppModel>>,
    sidebar_state: RwSignal<SidebarState>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let SidebarFilter::Tag(path) = filter else {
        return trigger.into_any();
    };
    let session = model.borrow().session_id();
    let open = create_rw_signal(false);
    let form = create_rw_signal(CategoryForm::Rename);
    let entries_model = model.clone();
    let entries_path = path.clone();
    let trigger = context_menu_view(
        trigger.into_view().style(|s| s.width_full().min_width(0.0)),
        palette,
        move || {
            if entries_model.borrow().session_id() != session
                || !entries_model.borrow().workspace.as_ref().is_some_and(|w| {
                    w.categories()
                        .iter()
                        .any(|c| category_path_is_same_or_descendant(&c.name, &entries_path))
                })
            {
                return vec![];
            }
            vec![
                entry(ICON_RENAME, i18n::Key::RenameCategory, true, move || {
                    form.set(CategoryForm::Rename);
                    open.set(true);
                }),
                entry(ICON_TRASH, i18n::Key::DeleteCategory, true, move || {
                    form.set(CategoryForm::Delete);
                    open.set(true);
                })
                .danger(true),
                entry(ICON_SORT, i18n::Key::SortNotes, true, move || {
                    form.set(CategoryForm::Sort);
                    open.set(true);
                }),
            ]
        },
    )
    .style(|s| s.width_full().min_width(0.0));
    anchored_popover(trigger, open, 360.0, 4.0, true, move || {
        match form.get_untracked() {
            CategoryForm::Rename => {
                let model = model.clone();
                let path = path.clone();
                edit_card(
                    i18n::Key::RenameCategory,
                    path.clone(),
                    open,
                    palette,
                    move |target| {
                        if model.borrow().session_id() != session {
                            return Err(msg!(SelectionNotOpen).into());
                        }
                        let success = model
                            .borrow_mut()
                            .rename_catalog_category(&path, target.trim());
                        changed(&model, revision);
                        if success {
                            Ok(())
                        } else {
                            Err(failure(&model))
                        }
                    },
                )
            }
            CategoryForm::Delete => {
                let model = model.clone();
                let path = path.clone();
                confirmation(
                    i18n::Key::DeleteCategory,
                    i18n::Key::DeleteCategoryHint,
                    open,
                    palette,
                    move || {
                        if model.borrow().session_id() != session {
                            return Err(msg!(SelectionNotOpen).into());
                        }
                        let success = model.borrow_mut().remove_catalog_category(&path);
                        changed(&model, revision);
                        if success {
                            Ok(())
                        } else {
                            Err(failure(&model))
                        }
                    },
                )
            }
            CategoryForm::Sort => {
                let scope = SidebarFilter::Tag(path.clone());
                let order_key = sidebar_note_order_key(&scope).expect("category has an order key");
                let current = sidebar_state.get_untracked().note_sort(order_key);
                sidebar_sort_popover(
                    scope,
                    model.clone(),
                    CategorySortPopoverSignals {
                        sidebar_state,
                        revision,
                        open,
                        field: create_rw_signal(current.field),
                        direction: create_rw_signal(current.direction),
                    },
                    palette,
                )
                .into_any()
            }
        }
    })
    .style(|s| s.width_full().min_width(0.0))
    .into_any()
}
