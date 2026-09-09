// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::ai_service::{Action, Failure};
use crate::*;
use stillus_ai::{
    AiEffort, AiError, AiModel, AiProfile, AiSettings, DEFAULT_ALIAS, detect_provider,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Connect,
    Refresh,
    Save,
    Remove,
    Disconnect,
    Cleanup,
}

#[derive(Clone)]
struct Controller {
    settings: RwSignal<AiSettings>,
    busy: RwSignal<bool>,
    operation: RwSignal<Operation>,
    feedback: RwSignal<Option<i18n::Key>>,
    connection_open: RwSignal<bool>,
    editing: RwSignal<Option<String>>,
    scroll_target: RwSignal<Option<ViewId>>,
    key: Rc<RefCell<Zeroizing<String>>>,
    key_revision: RwSignal<u64>,
    visible_key: RwSignal<bool>,
    validate_key: RwSignal<bool>,
    selected_key: RwSignal<bool>,
    generation: Rc<Cell<u64>>,
    global: Rc<RefCell<GlobalApplication>>,
    active: Rc<Cell<Option<ProjectionRequest>>>,
}

#[derive(Clone, Copy)]
struct ProjectionRequest {
    id: u64,
    generation: u64,
    save: bool,
    connecting: bool,
}

impl Controller {
    fn clear_key(&self) {
        self.key.borrow_mut().zeroize();
        self.key_revision.update(|revision| *revision += 1);
        self.visible_key.set(false);
        self.validate_key.set(false);
        self.selected_key.set(false);
    }

    fn paste_key(&self) {
        if self.busy.get_untracked() {
            return;
        }
        self.feedback.set(None);
        self.operation.set(Operation::Connect);
        self.validate_key.set(true);
        match Clipboard::get_contents() {
            Ok(value) => {
                let value = Zeroizing::new(value);
                if value.trim().len() <= 4096 {
                    self.key.borrow_mut().zeroize();
                    self.key.borrow_mut().push_str(value.trim());
                    self.selected_key.set(false);
                    self.key_revision.update(|revision| *revision += 1);
                } else {
                    self.feedback.set(Some(i18n::Key::AiKeyFormat));
                }
            }
            Err(_) => self.feedback.set(Some(i18n::Key::PastePasswordFailed)),
        }
    }

    fn connect(&self) {
        if self.busy.get_untracked() {
            return;
        }
        self.validate_key.set(true);
        if detect_provider(&self.key.borrow()).is_some() {
            let key = self.key.borrow().clone();
            self.submit(Action::Connect(key));
        }
    }

    fn submit(&self, action: Action) {
        if self.busy.get_untracked() {
            return;
        }
        self.operation.set(match &action {
            Action::Connect(_) => Operation::Connect,
            Action::Refresh => Operation::Refresh,
            Action::Save { .. } => Operation::Save,
            Action::Remove(_) => Operation::Remove,
            Action::Disconnect => Operation::Disconnect,
            Action::Cleanup => Operation::Cleanup,
        });
        let expected = self.settings.get_untracked();
        let save = matches!(action, Action::Save { .. } | Action::Remove(_));
        let connecting = matches!(action, Action::Connect(..));
        let generation = self.generation.get();
        let result = self.global.borrow_mut().start_ai(expected, action);
        match result {
            Ok(id) => {
                self.active.set(Some(ProjectionRequest {
                    id,
                    generation,
                    save,
                    connecting,
                }));
                self.busy.set(true);
                self.feedback.set(None);
            }
            Err(error) => self.feedback.set(Some(error_key(error))),
        }
    }
}

fn project(controller: &Controller) {
    let settings = controller.global.borrow().ai();
    if let Some(request) = controller.active.get() {
        let Some(result) = controller.global.borrow().ai_result(request.id) else {
            return;
        };
        controller.active.set(None);
        controller.busy.set(false);
        controller.settings.set(settings.clone());
        if controller.generation.get() != request.generation {
            return;
        }
        match result {
            Ok(_) => {
                controller
                    .feedback
                    .set(if controller.global.borrow().journal_blocked() {
                        Some(i18n::Key::AiJournalError)
                    } else if settings.pending_deletions.is_empty() {
                        None
                    } else {
                        Some(i18n::Key::AiCleanupError)
                    });
                controller
                    .connection_open
                    .set(settings.connection.is_none());
                if request.connecting {
                    controller.clear_key();
                }
                if request.connecting || request.save {
                    controller.editing.set(None);
                }
            }
            Err(error) => controller.feedback.set(Some(error_key(error))),
        }
    } else if controller.settings.get_untracked() != settings {
        // Results from tools use the same projection, including while the page is hidden.
        controller
            .connection_open
            .set(settings.connection.is_none());
        controller.settings.set(settings);
    }
}

fn error_key(error: Failure) -> i18n::Key {
    use i18n::Key as K;
    match error {
        Failure::Credentials => K::AiCredentialsError,
        Failure::Settings => K::AiSettingsError,
        Failure::Cancelled => K::Cancel,
        Failure::Cleanup => K::AiCleanupError,
        Failure::Api(error) => match error {
            AiError::AliasName => K::AiAliasNameError,
            AiError::AliasExists => K::AiAliasExists,
            AiError::DefaultAlias => K::AiDefaultProtected,
            AiError::KeyFormat => K::AiKeyFormat,
            AiError::Unauthorized => K::AiUnauthorized,
            AiError::Forbidden => K::AiForbidden,
            AiError::RateLimited => K::AiRateLimited,
            AiError::Network => K::AiNetworkError,
            AiError::Journal => K::AiJournalError,
            AiError::Response => K::AiResponseError,
            AiError::NoModels => K::AiNoModels,
            AiError::ModelUnavailable => K::AiUnavailable,
            AiError::EffortRequired => K::AiChooseEffort,
            AiError::Incomplete => K::AiConnectFirst,
        },
    }
}

/// The page heading of a settings section, matching the general and
/// encryption pages.
pub(super) fn page_title(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .font_size(26.0)
            .font_weight(floem::text::Weight::SEMIBOLD)
            .color(palette.ink)
            .selectable(false)
    })
}

pub(super) fn page_description(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string())
        .style(move |style| style.font_size(13.5).color(palette.muted).selectable(false))
}

/// A step of the page: the connection and the model aliases are two sections
/// of one page, titled like the cards of the general settings page.
pub(super) fn section_title(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .font_size(15.0)
            .font_weight(floem::text::Weight::SEMIBOLD)
            .color(palette.ink)
            .selectable(false)
    })
}

pub(super) fn spacer(height: f64) -> impl IntoView {
    empty().style(move |style| style.height(height))
}

/// One row of form actions. Buttons keep their own width instead of
/// stretching across the card, and wrap before they shrink.
pub(super) fn actions(children: impl ViewTuple + 'static) -> impl IntoView {
    h_stack(children).style(|style| {
        rtl_row(style)
            .gap(8.0)
            .items_center()
            .flex_wrap(floem::taffy::FlexWrap::Wrap)
    })
}

pub(super) fn page(
    signals: SettingsPageSignals,
    revision: RwSignal<u64>,
    global: Rc<RefCell<GlobalApplication>>,
    palette: Palette,
) -> impl IntoView {
    let journal_open = create_rw_signal(false);
    let journal = crate::ai_journal_view::page(global.clone(), journal_open, palette);
    let initial = global.borrow().ai();
    let controller = Controller {
        settings: create_rw_signal(initial),
        busy: create_rw_signal(false),
        operation: create_rw_signal(Operation::Connect),
        feedback: create_rw_signal(None),
        connection_open: create_rw_signal(true),
        editing: create_rw_signal(None),
        scroll_target: create_rw_signal(None),
        key: Rc::new(RefCell::new(Zeroizing::new(String::with_capacity(4096)))),
        key_revision: create_rw_signal(0),
        visible_key: create_rw_signal(false),
        validate_key: create_rw_signal(false),
        selected_key: create_rw_signal(false),
        generation: Rc::new(Cell::new(0)),
        global,
        active: Rc::new(Cell::new(None)),
    };
    let projection = controller.clone();
    create_effect(move |_| {
        revision.get();
        projection.busy.get();
        project(&projection);
    });
    let lifecycle = controller.clone();
    let was_visible = Rc::new(Cell::new(false));
    create_effect(move |_| {
        let visible = signals.open.get() && signals.section.get() == SettingsSection::Ai;
        if was_visible.replace(visible) == visible {
            return;
        }
        lifecycle.generation.set(lifecycle.generation.get() + 1);
        if lifecycle.active.get().is_some() {
            lifecycle.global.borrow_mut().cancel_ai();
        }
        lifecycle.clear_key();
        if !visible {
            journal_open.set(false);
        }
        lifecycle.feedback.set(None);
        lifecycle.operation.set(Operation::Connect);
        if visible {
            let diagnostic = lifecycle.global.borrow_mut().reload();
            let settings = lifecycle.global.borrow().ai();
            lifecycle.connection_open.set(settings.connection.is_none());
            lifecycle.settings.set(settings.clone());
            lifecycle.editing.set(None);
            if diagnostic.is_some() {
                lifecycle.feedback.set(Some(i18n::Key::AiSettingsError));
            }
        }
    });
    let settings = controller.settings;
    let busy = controller.busy;
    let connection_open = controller.connection_open;
    // Keep form signals alive while changing visibility. Rebuilding nested
    // dynamic containers can leave queued list updates referencing disposed drafts.
    let connection = v_stack((
        connection_form(controller.clone(), palette)
            .style(move |style| style.apply_if(!connection_open.get(), |style| style.hide())),
        connection_summary(controller.clone(), palette)
            .style(move |style| style.apply_if(connection_open.get(), |style| style.hide())),
    ))
    .style(|style| style.width_full());
    let cleanup_controller = controller.clone();
    let cleanup = actions((action_button(
        move || tr!(AiRetryCleanup),
        IconButtonTone::Danger,
        palette,
        move || !busy.get(),
        move || cleanup_controller.submit(Action::Cleanup),
    ),))
    .style(move |style| {
        style
            .margin_top(10.0)
            .apply_if(settings.get().pending_deletions.is_empty(), |style| {
                style.hide()
            })
    });
    let scroll_target = controller.scroll_target;
    let connection_section = v_stack((
        section_title(i18n::Key::AiConnect, palette),
        spacer(12.0),
        connection,
        feedback_view(controller.clone(), false, false, palette),
        feedback_view(controller.clone(), false, true, palette),
        cleanup,
    ))
    .style(|style| rtl_column(style).width_full());
    let models_section = aliases_section(controller.clone(), palette);
    let settings_page = scroll(
        v_stack((
            h_stack((
                page_title(i18n::Key::AiAssistant, palette),
                action_button(
                    move || tr!(AiJournal),
                    IconButtonTone::Secondary,
                    palette,
                    || true,
                    move || journal_open.set(true),
                ),
            ))
            .style(|s| rtl_row(s).items_center().justify_between().width_full()),
            spacer(7.0),
            page_description(i18n::Key::AiDescription, palette),
            spacer(28.0),
            connection_section,
            spacer(28.0),
            models_section,
        ))
        .style(|style| {
            rtl_column(style)
                .width_full()
                .padding_horiz(44.0)
                .padding_vert(38.0)
        }),
    )
    .scroll_to_view(move || scroll_target.get())
    .style(move |style| {
        style
            .min_width(0.0)
            .height_full()
            .flex_grow(1.0)
            .background(palette.canvas)
            .apply_if(journal_open.get(), |s| s.hide())
    });
    v_stack((settings_page, journal)).style(|s| s.width_full().height_full())
}

fn feedback_view(
    controller: Controller,
    alias: bool,
    cleanup: bool,
    palette: Palette,
) -> impl IntoView {
    let message = move || {
        let feedback = controller.feedback.get();
        let is_cleanup = feedback == Some(i18n::Key::AiCleanupError);
        let target = matches!(
            controller.operation.get(),
            Operation::Save | Operation::Remove
        );
        feedback.filter(|_| {
            !controller.busy.get()
                && if cleanup {
                    is_cleanup
                } else {
                    !is_cleanup && target == alias
                }
        })
    };
    label(move || message().map(|key| key.to_string()).unwrap_or_default()).style(move |style| {
        style
            .width_full()
            .max_width(SETTINGS_CARD_MAX_WIDTH_PX)
            .margin_top(10.0)
            .font_size(12.5)
            .line_height(1.4)
            .color(palette.danger)
            .selectable(false)
            .apply_if(message().is_none(), |style| style.hide())
    })
}

fn connection_summary(controller: Controller, palette: Palette) -> impl IntoView {
    let settings = controller.settings;
    let busy = controller.busy;
    let edit = controller.clone();
    let operation = controller.operation;
    let refresh = controller;
    v_stack((
        v_stack((
            label(move || {
                settings
                    .get()
                    .connection
                    .map(|connection| connection.provider.name().to_owned())
                    .unwrap_or_default()
            })
            .style(move |style| {
                style
                    .font_size(14.0)
                    .font_weight(floem::text::Weight::SEMIBOLD)
                    .color(palette.ink)
                    .selectable(false)
            }),
            label(move || tr!(AiKeySaved))
                .style(move |style| {
                    style
                        .font_size(12.5)
                        .color(palette.accent)
                        .selectable(false)
                })
                .tooltip(move || {
                    tooltip_label(
                        settings
                            .get()
                            .connection
                            .map(|connection| {
                                let date = chrono::DateTime::from_timestamp(
                                    connection.checked_at as i64,
                                    0,
                                )
                                .map(|time| {
                                    time.with_timezone(&chrono::Local)
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                })
                                .unwrap_or_default();
                                msg!(AiLastChecked, "value" => date).render()
                            })
                            .unwrap_or_default(),
                        palette,
                    )
                }),
        ))
        .style(|style| rtl_column(style).width_full().gap(4.0)),
        actions((
            action_button(
                move || tr!(AiChangeCredential),
                IconButtonTone::Secondary,
                palette,
                move || !busy.get(),
                move || {
                    edit.clear_key();
                    edit.feedback.set(None);
                    edit.connection_open.set(true);
                },
            ),
            action_button(
                move || {
                    if busy.get() && operation.get() == Operation::Refresh {
                        tr!(AiRefreshing)
                    } else {
                        tr!(AiRefreshModels)
                    }
                },
                IconButtonTone::Secondary,
                palette,
                move || !busy.get(),
                move || refresh.submit(Action::Refresh),
            ),
        )),
    ))
    .style(move |style| settings_card_style(style, palette).gap(14.0))
}

fn connection_form(controller: Controller, palette: Palette) -> impl IntoView {
    let busy = controller.busy;
    let settings = controller.settings;
    let revision = controller.key_revision;
    let validate = controller.validate_key;
    let operation = controller.operation;
    let provider_key = controller.key.clone();
    let provider_state = controller.key.clone();
    // One line under the field: the provider read from the key, or why the key
    // was not recognized.
    let provider_hint = label(move || {
        revision.get();
        let key = provider_key.borrow();
        if key.is_empty() {
            String::new()
        } else {
            detect_provider(&key)
                .map(|provider| provider.name().to_owned())
                .unwrap_or_else(|| {
                    if validate.get() {
                        tr!(AiKeyFormat)
                    } else {
                        String::new()
                    }
                })
        }
    })
    .style(move |style| {
        revision.get();
        let key = provider_state.borrow();
        let rejected = validate.get() && !key.is_empty() && detect_provider(&key).is_none();
        style
            .width_full()
            .font_size(12.5)
            .line_height(1.4)
            .selectable(false)
            .color(if rejected {
                palette.danger
            } else {
                palette.muted
            })
            .apply_if(
                key.is_empty() || (!validate.get() && detect_provider(&key).is_none()),
                |style| style.hide(),
            )
    });
    let warning_key = controller.key.clone();
    let warning_visible = controller.key.clone();
    let warning = label(move || {
        revision.get();
        let changed = settings.get().connection.is_some_and(|old| {
            detect_provider(&warning_key.borrow()).is_some_and(|provider| provider != old.provider)
        });
        if changed {
            tr!(AiProviderChange)
        } else {
            String::new()
        }
    })
    .style(move |style| {
        revision.get();
        let changed = settings.get().connection.is_some_and(|old| {
            detect_provider(&warning_visible.borrow())
                .is_some_and(|provider| provider != old.provider)
        });
        style
            .width_full()
            .font_size(12.5)
            .line_height(1.4)
            .color(palette.danger)
            .selectable(false)
            .apply_if(!changed, |style| style.hide())
    });
    let secret = secret_input(controller.clone(), palette);
    let submit = controller.clone();
    let cancel = controller.clone();
    let delete = controller.clone();
    let valid_key = controller.key.clone();
    v_stack((
        v_stack((settings_field_label(i18n::Key::AiKeyLabel, palette), secret))
            .style(|style| rtl_column(style).width_full().gap(7.0)),
        provider_hint,
        settings_hint(i18n::Key::AiKeyStorage, palette),
        warning,
        actions((
            action_button(
                move || {
                    if busy.get() && operation.get() == Operation::Connect {
                        tr!(AiConnecting)
                    } else if settings.get().connection.is_some() {
                        tr!(AiSaveCredential)
                    } else {
                        tr!(AiVerify)
                    }
                },
                IconButtonTone::Primary,
                palette,
                move || {
                    revision.get();
                    !busy.get() && detect_provider(&valid_key.borrow()).is_some()
                },
                move || submit.connect(),
            ),
            action_button(
                move || tr!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || !busy.get(),
                move || {
                    cancel.clear_key();
                    cancel.feedback.set(None);
                    cancel
                        .connection_open
                        .set(cancel.settings.get_untracked().connection.is_none());
                },
            )
            .style(move |style| {
                style.apply_if(settings.get().connection.is_none(), |style| style.hide())
            }),
            action_button(
                move || tr!(AiDisconnect),
                IconButtonTone::Danger,
                palette,
                move || !busy.get(),
                move || {
                    delete.clear_key();
                    delete.submit(Action::Disconnect);
                },
            )
            .style(move |style| {
                style.apply_if(settings.get().connection.is_none(), |style| style.hide())
            }),
        )),
    ))
    .style(move |style| settings_card_style(style, palette).gap(12.0))
}

fn secret_input(controller: Controller, palette: Palette) -> impl IntoView {
    const EYE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/></svg>"##;
    const EYE_OFF: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m3 3 18 18M10.6 5.1 12 5c6.5 0 10 7 10 7a19 19 0 0 1-3.2 4M6.5 6.5A20 20 0 0 0 2 12s3.5 7 10 7a11 11 0 0 0 5.5-1.5"/><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"/></svg>"##;
    let revision = controller.key_revision;
    let visible = controller.visible_key;
    let busy = controller.busy;
    let selected = controller.selected_key;
    let validate = controller.validate_key;
    let focused = create_rw_signal(false);
    let display = controller.key.clone();
    let empty_key = controller.key.clone();
    let input = controller.clone();
    let field = MaskedPasswordView::new(
        label(move || {
            revision.get();
            let key = display.borrow();
            if key.is_empty() {
                tr!(AiPasteCredential)
            } else if visible.get() {
                key.to_string()
            } else {
                "•".repeat(key.len().min(32))
            }
        })
        .style(|style| style.min_width(0.0).max_width_full().selectable(false)),
        || {},
        move |event| {
            if busy.get_untracked() {
                return EventPropagation::Stop;
            }
            let Event::KeyDown(event) = event else {
                return EventPropagation::Stop;
            };
            let shortcut = event.modifiers.meta() || event.modifiers.control();
            match &event.key.logical_key {
                Key::Named(NamedKey::Tab) => return EventPropagation::Continue,
                Key::Named(NamedKey::Enter) => {
                    input.connect();
                    return EventPropagation::Stop;
                }
                Key::Character(value) if shortcut && value.eq_ignore_ascii_case("v") => {
                    input.paste_key();
                    return EventPropagation::Stop;
                }
                Key::Character(value) if shortcut && value.eq_ignore_ascii_case("a") => {
                    selected.set(true);
                    return EventPropagation::Stop;
                }
                Key::Named(NamedKey::Escape) => input.clear_key(),
                Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) => {
                    if selected.get_untracked() {
                        input.key.borrow_mut().zeroize();
                    } else {
                        input.key.borrow_mut().pop();
                    }
                    selected.set(false);
                }
                Key::Character(value) if !shortcut => {
                    if selected.get_untracked() {
                        input.key.borrow_mut().zeroize();
                    }
                    selected.set(false);
                    let mut entry = input.key.borrow_mut();
                    if entry.len() + value.len() <= 4096
                        && value.is_ascii()
                        && !value.chars().any(char::is_control)
                    {
                        entry.push_str(value);
                    }
                }
                _ => return EventPropagation::Stop,
            }
            validate.set(false);
            input.feedback.set(None);
            revision.update(|r| *r += 1);
            EventPropagation::Stop
        },
    )
    .style(move |style| {
        revision.get();
        style
            .min_width(0.0)
            .flex_grow(1.0)
            .height_full()
            .items_center()
            .cursor(CursorStyle::Text)
            .font_size(13.5)
            .color(if empty_key.borrow().is_empty() {
                palette.muted
            } else {
                palette.ink
            })
            .apply_if(selected.get(), |style| {
                style.background(palette.accent_soft)
            })
    })
    .on_event_stop(EventListener::FocusGained, move |_| focused.set(true))
    .on_event_stop(EventListener::FocusLost, move |_| {
        focused.set(false);
        validate.set(true);
    })
    .keyboard_navigable();
    let reveal_key = controller.key.clone();
    let reveal_enabled = move || {
        revision.get();
        !busy.get() && !reveal_key.borrow().is_empty()
    };
    let reveal_press_enabled = reveal_enabled.clone();
    let reveal = reliable_button(
        svg(EYE)
            .update_value(move || if visible.get() { EYE_OFF } else { EYE })
            .style(|style| style.size(16.0, 16.0)),
        move || {
            if reveal_press_enabled() {
                visible.update(|show| *show = !*show);
            }
        },
    )
    .style(move |style| {
        style
            .size(28.0, 28.0)
            .items_center()
            .justify_center()
            .border_radius(4.0)
            .color(if reveal_enabled() {
                palette.ink
            } else {
                palette.muted
            })
            .cursor(if reveal_enabled() {
                CursorStyle::Pointer
            } else {
                CursorStyle::Default
            })
            .hover(|style| style.background(palette.canvas))
            .focus(|style| style.outline(1.0).outline_color(palette.accent))
    })
    .tooltip(move || {
        tooltip_label(
            if visible.get() {
                tr!(AiConcealCredential)
            } else {
                tr!(AiRevealCredential)
            },
            palette,
        )
    });
    h_stack((
        h_stack((field, reveal)).style(move |style| {
            // Keys and the reveal control retain left-to-right order in RTL locales.
            settings_control_style(style, palette)
                .flex_row()
                .flex_grow(1.0)
                .width(0.0)
                .gap(8.0)
                .padding_right(5.0)
                .border_color(if focused.get() {
                    palette.accent
                } else {
                    palette.divider
                })
        }),
        action_button(
            move || tr!(AiPaste),
            IconButtonTone::Secondary,
            palette,
            move || !busy.get(),
            move || controller.paste_key(),
        ),
    ))
    .style(|style| rtl_row(style).width_full().items_center().gap(8.0))
}

fn aliases_section(controller: Controller, palette: Palette) -> impl IntoView {
    let settings = controller.settings;
    let editing = controller.editing;
    let busy = controller.busy;
    let feedback = controller.feedback;
    let rows = dyn_stack(
        move || {
            let mut names: Vec<_> =
                settings.with(|settings| settings.aliases.keys().cloned().collect());
            names.sort_by(|a, b| (a != DEFAULT_ALIAS, a).cmp(&(b != DEFAULT_ALIAS, b)));
            names
        },
        |name| name.clone(),
        move |name| {
            let title = name.clone();
            let summary_name = name.clone();
            let valid_name = name.clone();
            let hidden_name = name.clone();
            reliable_button(
                h_stack((
                    v_stack((
                        label(move || title.clone()).style(move |style| {
                            style
                                .width_full()
                                .min_width(0.0)
                                .font_size(14.0)
                                .color(palette.ink)
                                .text_ellipsis()
                                .selectable(false)
                        }),
                        label(move || {
                            settings.with(|settings| {
                                let Some(profile) = settings.aliases.get(&summary_name) else {
                                    return String::new();
                                };
                                let model = settings
                                    .connection
                                    .as_ref()
                                    .and_then(|c| c.models.iter().find(|m| m.id == profile.model));
                                let name = model.map(|m| m.name.as_str()).unwrap_or(&profile.model);
                                let effort = profile
                                    .effort
                                    .map(|e| e.name().to_owned())
                                    .unwrap_or_else(|| tr!(AiManaged));
                                if settings.validate_profile(profile).is_ok() {
                                    format!("{name} · {effort}")
                                } else {
                                    format!("{name} · {}", tr!(AiUnavailable))
                                }
                            })
                        })
                        .style(move |style| {
                            let valid = settings.with(|settings| {
                                settings
                                    .aliases
                                    .get(&valid_name)
                                    .is_some_and(|p| settings.validate_profile(p).is_ok())
                            });
                            style
                                .width_full()
                                .min_width(0.0)
                                .font_size(12.0)
                                .text_ellipsis()
                                .color(if valid { palette.muted } else { palette.danger })
                                .selectable(false)
                        }),
                    ))
                    .style(|style| rtl_column(style).flex_grow(1.0).min_width(0.0).gap(4.0)),
                    svg(ICON_CHEVRON_RIGHT)
                        .update_value(move || {
                            if i18n::current().is_rtl() {
                                ICON_BACK
                            } else {
                                ICON_CHEVRON_RIGHT
                            }
                        })
                        .style(move |style| style.size(14.0, 14.0).color(palette.muted)),
                ))
                .style(|style| rtl_row(style).width_full().items_center().gap(12.0)),
                move || {
                    if !busy.get_untracked() {
                        feedback.set(None);
                        editing.set(Some(name.clone()));
                    }
                },
            )
            .style(move |style| {
                settings_card_style(style, palette)
                    .cursor(CursorStyle::Pointer)
                    .focus(|style| style.outline(1.0).outline_color(palette.accent))
                    .apply_if(editing.get().as_ref() == Some(&hidden_name), |style| {
                        style.hide()
                    })
            })
        },
    )
    .style(|style| rtl_column(style).flex_col().width_full().gap(12.0));
    // One persistent editor avoids disposing nested model lists when an alias is
    // added, renamed, or removed while Floem still has queued signal updates.
    let editor = v_stack((
        section_title(i18n::Key::AiAliasEdit, palette),
        alias_form(controller.clone(), palette),
    ))
    .style(move |style| {
        settings_card_style(style, palette)
            .gap(16.0)
            .border_color(palette.accent)
            .apply_if(editing.get().is_none(), |style| style.hide())
    });
    let editor_id = editor.id();
    let scroll_target = controller.scroll_target;
    create_effect(move |_| {
        if editing.get().is_some() {
            exec_after(Duration::from_millis(100), move |_| {
                if editing.get_untracked().is_some() {
                    scroll_target.set(Some(editor_id));
                }
            });
        }
    });
    v_stack((
        h_stack((
            section_title(i18n::Key::AiModels, palette),
            action_button(
                move || tr!(AiAliasAdd),
                IconButtonTone::Secondary,
                palette,
                move || !busy.get() && editing.get().is_none(),
                move || {
                    feedback.set(None);
                    editing.set(Some(String::new()));
                },
            ),
        ))
        .style(|style| {
            rtl_row(style)
                .width_full()
                .items_center()
                .justify_between()
                .gap(12.0)
                .flex_wrap(floem::taffy::FlexWrap::Wrap)
        }),
        settings_hint(i18n::Key::AiAliasesHint, palette),
        editor,
        rows,
    ))
    .style(move |style| {
        rtl_column(style)
            .width_full()
            .max_width(SETTINGS_CARD_MAX_WIDTH_PX)
            .gap(12.0)
            .apply_if(settings.get().connection.is_none(), |style| style.hide())
    })
}

pub(crate) fn alias_dropdown<T: Clone + 'static>(
    value: RwSignal<Option<T>>,
    items: Vec<T>,
    display: impl Fn(Option<T>) -> String + 'static,
    accept: impl Fn(T) + 'static,
    enabled: impl Fn() -> bool + 'static,
    palette: Palette,
) -> impl IntoView {
    let display = Rc::new(display);
    let item_display = display.clone();
    floem::views::dropdown::Dropdown::custom(
        move || value.get(),
        move |item| {
            let display = display.clone();
            h_stack((
                label(move || display(item.clone()))
                    .style(|style| style.min_width(0.0).text_ellipsis().selectable(false)),
                svg(ICON_CHEVRON_DOWN).style(|style| style.size(12.0, 12.0)),
            ))
            .style(|style| {
                rtl_row(style)
                    .width_full()
                    .items_center()
                    .justify_between()
                    .gap(8.0)
            })
            .into_any()
        },
        items.into_iter().map(Some).collect::<Vec<_>>(),
        move |item| {
            let display = item_display.clone();
            label(move || display(item.clone()))
                .style(move |style| {
                    style
                        .width_full()
                        .height(34.0)
                        .padding_horiz(12.0)
                        .items_center()
                        .font_size(13.0)
                        .color(palette.ink)
                        .background(palette.paper)
                        .selectable(false)
                        .hover(|style| style.background(palette.accent_soft))
                        .focus(|style| style.background(palette.accent_soft))
                })
                .into_any()
        },
    )
    .on_accept(move |item| {
        if let Some(item) = item {
            accept(item);
        }
    })
    .disabled(move || !enabled())
    .keyboard_navigable()
    .style(move |style| {
        settings_control_style(style, palette)
            .cursor(CursorStyle::Pointer)
            .focus(|style| style.border_color(palette.accent))
            .disabled(|style| style.color(palette.muted).cursor(CursorStyle::Default))
            .class(floem::views::scroll::ScrollClass, |style| {
                style
                    .width_full()
                    .max_height(204.0)
                    .background(palette.paper)
                    .border(1.0)
                    .border_color(palette.divider)
                    .border_radius(6.0)
            })
    })
}

fn selected_effort(model: &AiModel, previous: Option<AiEffort>) -> Option<AiEffort> {
    previous
        .filter(|effort| model.efforts.contains(effort))
        .or_else(|| {
            model
                .efforts
                .contains(&AiEffort::High)
                .then_some(AiEffort::High)
        })
        .or_else(|| model.efforts.first().copied())
}

fn alias_draft(settings: &AiSettings, name: &str) -> Option<AiProfile> {
    let models = &settings.connection.as_ref()?.models;
    let saved = settings
        .aliases
        .get(name)
        .or_else(|| settings.aliases.get(DEFAULT_ALIAS));
    let model = models
        .iter()
        .find(|model| {
            !model.efforts.is_empty() && saved.is_some_and(|saved| saved.model == model.id)
        })
        .or_else(|| models.iter().find(|model| !model.efforts.is_empty()))?;
    Some(AiProfile {
        model: model.id.clone(),
        effort: selected_effort(model, saved.and_then(|saved| saved.effort)),
    })
}

fn alias_form(controller: Controller, palette: Palette) -> impl IntoView {
    let settings = controller.settings;
    let busy = controller.busy;
    let name = create_rw_signal(String::new());
    let model = create_rw_signal(None::<String>);
    let effort = create_rw_signal(None);
    let changed_effort = create_rw_signal(false);
    let editing = controller.editing;
    create_effect(move |_| {
        let current = editing.get();
        name.set(current.clone().unwrap_or_default());
        let saved =
            settings.with_untracked(|s| alias_draft(s, current.as_deref().unwrap_or_default()));
        model.set(saved.as_ref().map(|profile| profile.model.clone()));
        effort.set(saved.and_then(|profile| profile.effort));
        changed_effort.set(false);
    });
    let available = move || {
        settings.get().connection.and_then(|c| {
            c.models
                .into_iter()
                .find(|m| Some(&m.id) == model.get().as_ref())
        })
    };
    let model_picker = dyn_container(
        move || {
            settings.with(|s| {
                s.connection
                    .as_ref()
                    .map(|c| {
                        c.models
                            .iter()
                            .filter(|m| !m.efforts.is_empty())
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
        },
        move |models| {
            alias_dropdown(
                model,
                models.iter().map(|m| m.id.clone()).collect(),
                move |value| {
                    settings.with(|s| {
                        s.connection
                            .as_ref()
                            .and_then(|c| c.models.iter().find(|m| Some(&m.id) == value.as_ref()))
                            .map(|m| m.name.clone())
                            .unwrap_or_else(|| value.unwrap_or_else(|| tr!(AiChooseModel)))
                    })
                },
                move |id| {
                    let previous = effort.get_untracked();
                    let next = models
                        .iter()
                        .find(|m| m.id == id)
                        .and_then(|model| selected_effort(model, previous));
                    effort.set(next);
                    changed_effort.set(previous.is_some() && previous != next);
                    model.set(Some(id));
                },
                move || !busy.get(),
                palette,
            )
        },
    )
    .style(|style| style.width_full());
    let effort_picker = dyn_container(
        move || available().map(|m| m.efforts).unwrap_or_default(),
        move |efforts| {
            let has_efforts = !efforts.is_empty();
            alias_dropdown(
                effort,
                efforts,
                move |value| {
                    value
                        .map(|e| e.name().to_owned())
                        .unwrap_or_else(|| tr!(AiChooseEffort))
                },
                move |value| {
                    effort.set(Some(value));
                    changed_effort.set(false);
                },
                move || !busy.get() && has_efforts,
                palette,
            )
        },
    )
    .style(|style| style.width_full());
    let save = controller.clone();
    let operation = controller.operation;
    let feedback = controller.clone();
    let remove = controller.clone();
    let cancel = controller;
    v_stack((
        v_stack((
            settings_field_label(i18n::Key::AiAliasName, palette),
            localized_input::LocalizedInput::new(name, i18n::Key::AiAliasNamePlaceholder).style(
                move |style| {
                    settings_input_style(style, palette)
                        .apply_if(editing.get().as_deref() == Some(DEFAULT_ALIAS), |style| {
                            style.hide()
                        })
                },
            ),
            label(move || DEFAULT_ALIAS.to_owned()).style(move |style| {
                style
                    .font_size(13.0)
                    .color(palette.ink)
                    .apply_if(editing.get().as_deref() != Some(DEFAULT_ALIAS), |style| {
                        style.hide()
                    })
            }),
            label(move || {
                if name.get().is_empty() {
                    return String::new();
                }
                let old = editing.get().filter(|old| !old.is_empty());
                settings
                    .get()
                    .validate_alias_name(old.as_deref(), name.get().trim())
                    .err()
                    .map(|error| error_key(Failure::Api(error)).to_string())
                    .unwrap_or_default()
            })
            .style(move |style| {
                let old = editing.get().filter(|old| !old.is_empty());
                style
                    .width_full()
                    .font_size(12.5)
                    .color(palette.danger)
                    .apply_if(
                        name.get().is_empty()
                            || settings
                                .get()
                                .validate_alias_name(old.as_deref(), name.get().trim())
                                .is_ok(),
                        |style| style.hide(),
                    )
            }),
        ))
        .style(|style| rtl_column(style).width_full().gap(7.0)),
        settings_hint(i18n::Key::AiNoModels, palette)
            .style(move |style| style.apply_if(model.get().is_some(), |style| style.hide())),
        v_stack((
            settings_field_label(i18n::Key::AiModelLabel, palette),
            model_picker,
        ))
        .style(move |style| {
            rtl_column(style)
                .width_full()
                .gap(7.0)
                .apply_if(model.get().is_none(), |style| style.hide())
        }),
        v_stack((
            settings_field_label(i18n::Key::AiEffortLabel, palette),
            effort_picker,
            label(move || tr!(AiEffortReset))
                .style(move |style| {
                    style
                        .width_full()
                        .font_size(12.5)
                        .line_height(1.4)
                        .color(palette.danger)
                        .selectable(false)
                })
                .style(move |style| style.apply_if(!changed_effort.get(), |style| style.hide())),
        ))
        .style(move |style| {
            rtl_column(style)
                .width_full()
                .gap(8.0)
                .apply_if(model.get().is_none(), |style| style.hide())
        }),
        actions((
            action_button(
                move || {
                    if busy.get() && operation.get() == Operation::Save {
                        tr!(AiSaving)
                    } else {
                        tr!(Save)
                    }
                },
                IconButtonTone::Primary,
                palette,
                move || {
                    let old = editing.get().filter(|old| !old.is_empty());
                    !busy.get()
                        && settings
                            .get()
                            .validate_alias_name(old.as_deref(), name.get().trim())
                            .is_ok()
                        && model.get().is_some_and(|model| {
                            settings
                                .get()
                                .validate_profile(&AiProfile {
                                    model,
                                    effort: effort.get(),
                                })
                                .is_ok()
                        })
                },
                move || {
                    if let Some(model) = model.get_untracked() {
                        save.submit(Action::Save {
                            old: editing.get_untracked().filter(|old| !old.is_empty()),
                            name: name.get_untracked().trim().to_owned(),
                            profile: AiProfile {
                                model,
                                effort: effort.get_untracked(),
                            },
                        });
                    }
                },
            ),
            action_button(
                move || tr!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || !busy.get(),
                move || {
                    cancel.feedback.set(None);
                    cancel.editing.set(None);
                },
            ),
        )),
        settings_hint(i18n::Key::AiAliasDeleteHint, palette).style(move |style| {
            style.apply_if(
                editing
                    .get()
                    .as_deref()
                    .is_none_or(|name| name.is_empty() || name == DEFAULT_ALIAS),
                |style| style.hide(),
            )
        }),
        actions((action_button(
            move || tr!(AiAliasDelete),
            IconButtonTone::Danger,
            palette,
            move || !busy.get(),
            move || {
                if let Some(name) = editing
                    .get_untracked()
                    .filter(|name| !name.is_empty() && name != DEFAULT_ALIAS)
                {
                    remove.submit(Action::Remove(name));
                }
            },
        ),))
        .style(move |style| {
            style.apply_if(
                editing
                    .get()
                    .as_deref()
                    .is_none_or(|name| name.is_empty() || name == DEFAULT_ALIAS),
                |style| style.hide(),
            )
        }),
        feedback_view(feedback, true, false, palette),
    ))
    .style(|style| rtl_column(style).width_full().gap(14.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, efforts: Vec<AiEffort>) -> AiModel {
        AiModel {
            id: id.into(),
            name: id.into(),
            efforts,
        }
    }

    #[test]
    fn model_changes_always_select_a_supported_effort() {
        let model = model("test", vec![AiEffort::Low, AiEffort::High]);
        assert_eq!(selected_effort(&model, None), Some(AiEffort::High));
        assert_eq!(
            selected_effort(&model, Some(AiEffort::Low)),
            Some(AiEffort::Low)
        );
        assert_eq!(
            selected_effort(&model, Some(AiEffort::Max)),
            Some(AiEffort::High)
        );
        let restricted = self::model("restricted", vec![AiEffort::Low]);
        assert_eq!(
            selected_effort(&restricted, Some(AiEffort::High)),
            Some(AiEffort::Low)
        );
    }

    #[test]
    fn new_alias_inherits_default_and_excludes_models_without_effort() {
        let mut settings = AiSettings::default();
        settings.connect(stillus_ai::AiConnection {
            provider: stillus_ai::AiProvider::OpenAi,
            credential: "fixture".into(),
            checked_at: 1,
            models: vec![
                model("managed", vec![]),
                model("custom", vec![AiEffort::Low, AiEffort::High]),
            ],
        });
        let fallback = alias_draft(&settings, "new").unwrap();
        assert_eq!(fallback.model, "custom");
        assert_eq!(fallback.effort, Some(AiEffort::High));
        let custom = AiProfile {
            model: "custom".into(),
            effort: Some(AiEffort::Low),
        };
        settings
            .aliases
            .insert(DEFAULT_ALIAS.into(), custom.clone());
        assert_eq!(alias_draft(&settings, "new"), Some(custom));
        settings
            .connection
            .as_mut()
            .unwrap()
            .models
            .retain(|model| model.efforts.is_empty());
        assert!(alias_draft(&settings, "new").is_none());
    }
}
