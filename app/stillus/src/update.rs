// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! The update settings page, the startup check and the prompt that offers a
//! published release.
//!
//! Checking and installing run on a worker thread; the user interface only
//! reads the stage the worker reports. An automatic check holds a release back
//! for a day after publication, a manual check never does, and nothing is
//! installed without the user pressing the button.

use crate::*;
#[cfg(test)]
use stillus_update::Version;
use stillus_update::{CheckMode, Installation, UpdateError};

const POLL_MS: u64 = 60;
/// Distance between the prompt card and the window corner.
const PROMPT_INSET_PX: f64 = 18.0;

use crate::application::updates::Stage;

#[derive(Clone)]
pub(crate) struct Updates {
    stage: RwSignal<Stage>,
    /// The prompt that appears over the workspace after a startup check.
    prompt: RwSignal<bool>,
    automatic: RwSignal<bool>,
    installation: Rc<Option<Installation>>,
    global: Rc<RefCell<GlobalApplication>>,
    restart: RestartAction,
    restarting: RwSignal<bool>,
    restart_error: RwSignal<Option<UiText>>,
}

pub(crate) type RestartAction = Rc<dyn Fn(&Installation) -> Result<bool, UiText>>;

impl Updates {
    pub(crate) fn new(global: Rc<RefCell<GlobalApplication>>, restart: RestartAction) -> Self {
        let installation = global.borrow().update.installation.clone();
        let settings = global.borrow().updates();
        let stage = global.borrow().update.stage.clone();
        Self {
            stage: create_rw_signal(stage),
            prompt: create_rw_signal(false),
            automatic: create_rw_signal(settings.automatic),
            installation: Rc::new(installation),
            global,
            restart,
            restarting: create_rw_signal(false),
            restart_error: create_rw_signal(None),
        }
    }

    /// UI observes global application progress; it does not own worker channels.
    pub(crate) fn start(&self) {
        schedule(self.clone());
    }
    fn check(&self, mode: CheckMode) {
        self.global.borrow_mut().update.check(mode);
        self.project();
    }
    fn install(&self) {
        self.global.borrow_mut().update.install();
        self.project();
    }
    fn project(&self) {
        let global = self.global.borrow();
        if self.stage.get_untracked() != global.update.stage {
            self.stage.set(global.update.stage.clone());
        }
        if self.prompt.get_untracked() != global.update.prompt {
            self.prompt.set(global.update.prompt);
        }
        let automatic = global.updates().automatic;
        if self.automatic.get_untracked() != automatic {
            self.automatic.set(automatic);
        }
    }

    /// Remembers that this version was declined, so the prompt does not
    /// reappear at every start until a newer release is published.
    fn dismiss(&self) {
        self.global.borrow_mut().update.prompt = false;
        self.prompt.set(false);
        self.restarting.set(false);
        let Some(release) = self.stage.get_untracked().release().cloned() else {
            return;
        };
        let mut settings = self.global.borrow().updates();
        settings.dismissed = Some(release.version.to_string());
        self.store(settings);
    }

    fn set_automatic(&self, automatic: bool) {
        self.automatic.set(automatic);
        let mut settings = self.global.borrow().updates();
        settings.automatic = automatic;
        self.store(settings);
    }

    fn store(&self, settings: UpdateSettings) {
        if let Err(error) = self.global.borrow_mut().set_updates(settings) {
            self.stage
                .set(Stage::Failed(UpdateError::Io(error.to_string())));
        }
    }

    fn open_page(&self) {
        let Some(release) = self.stage.get_untracked().release().cloned() else {
            return;
        };
        if let Err(error) = open_rss_original(&release.page_url) {
            self.stage
                .set(Stage::Failed(UpdateError::Io(error.to_string())));
        }
    }

    fn request_restart(&self) {
        if !matches!(self.stage.get_untracked(), Stage::Installed(_))
            || self.restarting.get_untracked()
        {
            return;
        }
        self.restart_error.set(None);
        self.restarting.set(true);
        self.restart_tick();
    }

    fn restart_tick(&self) {
        if self.restarting.try_get_untracked() != Some(true) {
            return;
        }
        let Some(installation) = self.installation.as_ref() else {
            self.restarting.set(false);
            self.restart_error
                .set(Some(msg!(UpdateNotInstalled).into()));
            return;
        };
        match (self.restart)(installation) {
            Ok(true) => {}
            Ok(false) => {
                let controller = self.clone();
                exec_after(Duration::from_millis(POLL_MS), move |_| {
                    controller.restart_tick()
                });
            }
            Err(error) => {
                self.restarting.set(false);
                self.restart_error.set(Some(error));
            }
        }
    }
}

fn schedule(controller: Updates) {
    exec_after(Duration::from_millis(POLL_MS), move |_| {
        if controller.stage.try_get_untracked().is_none() {
            return;
        }
        controller.project();
        schedule(controller);
    });
}

/// The prompt that offers a release found by the startup check. It never
/// blocks the workspace: the window stays usable behind it.
pub(crate) fn prompt_view(updates: Updates, palette: Palette) -> impl IntoView {
    let stage = updates.stage;
    let prompt = updates.prompt;
    let install = updates.clone();
    let later = updates.clone();
    let restart = updates.clone();
    let restarting = updates.restarting;
    let restart_error = updates.restart_error;
    let title = label(move || match stage.get() {
        Stage::Installed(version) => {
            UiText::from(msg!(UpdateInstalledRestart, "version" => version.to_string()))
        }
        stage => stage.release().map_or_else(UiText::default, |release| {
            UiText::from(msg!(UpdateAvailable, "version" => release.version.to_string()))
        }),
    })
    .style(move |style| {
        style
            .font_size(crate::ui::FONT_BODY as f32)
            .color(palette.ink)
            .selectable(false)
    });
    let status = label(move || {
        restart_error.get().unwrap_or_else(|| {
            if restarting.get() {
                msg!(UpdateRestartWaiting).into()
            } else {
                status_text(&stage.get())
            }
        })
    })
    .style(move |style| {
        style
            .font_size(crate::ui::FONT_CAPTION as f32)
            .color(palette.muted)
            .selectable(false)
    })
    .style(move |style| {
        style.apply_if(
            matches!(stage.get(), Stage::Available(_))
                || (matches!(stage.get(), Stage::Installed(_))
                    && !restarting.get()
                    && restart_error.get().is_none()),
            |style| style.hide(),
        )
    });
    let update_button = form_action_button(
        ButtonAction::Custom(ButtonAction::Download.icon()),
        move || tr!(UpdateInstall),
        IconButtonTone::Primary,
        palette,
        move || matches!(stage.get(), Stage::Available(_)),
        move || install.install(),
    )
    .style(move |style| {
        style.apply_if(
            !matches!(stage.get(), Stage::Available(_) | Stage::Downloading { .. }),
            |style| style.hide(),
        )
    });
    let dismiss = form_action_button(
        ButtonAction::Custom(ICON_CANCEL),
        move || tr!(UpdateLater),
        IconButtonTone::Secondary,
        palette,
        || true,
        move || later.dismiss(),
    );
    let restart_button = form_action_button(
        ButtonAction::Custom(ICON_UPDATE),
        move || tr!(UpdateRestart),
        IconButtonTone::Primary,
        palette,
        move || !restarting.get(),
        move || restart.request_restart(),
    )
    .style(move |style| {
        style.apply_if(!matches!(stage.get(), Stage::Installed(_)), |style| {
            style.hide()
        })
    });
    let card = v_stack((
        title,
        status,
        h_stack((
            empty().style(|style| style.flex_grow(1.0)),
            dismiss,
            update_button,
            restart_button,
        ))
        .style(|style| rtl_row(style).width_full().items_center().gap(8.0)),
    ))
    .style(move |style| {
        rtl_column(style)
            .width(330.0)
            .gap(10.0)
            .padding(16.0)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(9.0)
    });
    // The card is placed in the corner of the window instead of inside a
    // full-window container, so that only the card itself sits over the
    // workspace and the rest of the window keeps receiving pointer events.
    card.style(move |style| {
        let style = style
            .absolute()
            .inset_bottom(PROMPT_INSET_PX)
            .apply_if(i18n::current().is_rtl(), |style| {
                style.inset_left(PROMPT_INSET_PX)
            })
            .apply_if(!i18n::current().is_rtl(), |style| {
                style.inset_right(PROMPT_INSET_PX)
            });
        if prompt.get() { style } else { style.hide() }
    })
}

/// The settings page: the manual check, the result and the startup switch.
pub(crate) fn page(
    signals: SettingsPageSignals,
    updates: Updates,
    palette: Palette,
) -> impl IntoView {
    let stage = updates.stage;
    let automatic = updates.automatic;
    // Leaving the page cancels nothing, but a finished result should not be
    // presented as fresh the next time the page opens.
    let lifecycle = updates.clone();
    let was_visible = Rc::new(Cell::new(false));
    create_effect(move |_| {
        let visible = signals.open.get() && signals.section.get() == SettingsSection::Updates;
        if was_visible.replace(visible) == visible || visible {
            return;
        }
        if matches!(lifecycle.stage.get_untracked(), Stage::Failed(_)) {
            lifecycle.stage.set(Stage::Idle);
        }
    });

    let check = updates.clone();
    let install = updates.clone();
    let page_open = updates.clone();
    let toggle = updates.clone();
    let restart = updates.clone();
    let restarting = updates.restarting;
    let restart_error = updates.restart_error;
    let status_card = v_stack((
        label(|| msg!(UpdateInstalledVersion, "version" => env!("CARGO_PKG_VERSION"))).style(
            move |style| {
                style
                    .font_size(crate::ui::FONT_CARD as f32)
                    .color(palette.ink)
                    .selectable(false)
            },
        ),
        label(move || status_text(&stage.get())).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .line_height(1.4)
                .color(palette.muted)
        }),
        label(move || restart_error.get().unwrap_or_default()).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
                .apply_if(restart_error.get().is_none(), |style| style.hide())
        }),
        actions((
            form_action_button(
                ButtonAction::Custom(ButtonAction::Refresh.icon()),
                move || tr!(UpdateCheckNow),
                IconButtonTone::Secondary,
                palette,
                move || {
                    !stage.get().busy()
                        && !matches!(stage.get(), Stage::Unsupported(_) | Stage::Installed(_))
                },
                move || check.check(CheckMode::Manual),
            ),
            form_action_button(
                ButtonAction::Custom(ButtonAction::Download.icon()),
                move || tr!(UpdateInstall),
                IconButtonTone::Primary,
                palette,
                move || matches!(stage.get(), Stage::Available(_) | Stage::Held(_)),
                move || install.install(),
            )
            .style(move |style| {
                style.apply_if(
                    !matches!(
                        stage.get(),
                        Stage::Available(_) | Stage::Held(_) | Stage::Downloading { .. }
                    ),
                    |style| style.hide(),
                )
            }),
            form_action_button(
                ButtonAction::Custom(ICON_FILE),
                move || tr!(UpdateOpenPage),
                IconButtonTone::Secondary,
                palette,
                || true,
                move || page_open.open_page(),
            )
            .style(move |style| {
                style.apply_if(stage.get().release().is_none(), |style| style.hide())
            }),
            form_action_button(
                ButtonAction::Custom(ICON_UPDATE),
                move || tr!(UpdateRestart),
                IconButtonTone::Primary,
                palette,
                move || !restarting.get(),
                move || {
                    restart.prompt.set(true);
                    restart.request_restart();
                },
            )
            .style(move |style| {
                style.apply_if(!matches!(stage.get(), Stage::Installed(_)), |style| {
                    style.hide()
                })
            }),
        ))
        .style(|style| style.margin_top(4.0)),
    ))
    .style(move |style| settings_card_style(style, palette).gap(10.0));

    let notes_card = v_stack((
        settings_field_label(i18n::Key::UpdateNotes, palette),
        label(move || {
            stage
                .get()
                .release()
                .map(|release| release.notes.clone())
                .unwrap_or_default()
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .line_height(1.4)
                .color(palette.muted)
        }),
    ))
    .style(move |style| {
        settings_card_style(style, palette).gap(8.0).apply_if(
            stage
                .get()
                .release()
                .is_none_or(|release| release.notes.is_empty()),
            |style| style.hide(),
        )
    });

    let automatic_card = v_stack((
        label(move || tr!(UpdateAutomatic)).style(move |style| {
            style
                .font_size(crate::ui::FONT_CARD as f32)
                .color(palette.ink)
                .selectable(false)
        }),
        settings_hint(i18n::Key::UpdateAutomaticHint, palette),
        label(move || {
            if automatic.get() {
                tr!(UpdateAutomaticEnabled)
            } else {
                tr!(UpdateAutomaticDisabled)
            }
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
                .selectable(false)
        }),
        actions((form_action_button(
            ButtonAction::Custom(ICON_UPDATE),
            move || {
                if automatic.get() {
                    tr!(UpdateAutomaticDisable)
                } else {
                    tr!(UpdateAutomaticEnable)
                }
            },
            IconButtonTone::Secondary,
            palette,
            || true,
            move || toggle.set_automatic(!automatic.get_untracked()),
        ),))
        .style(|style| style.margin_top(4.0)),
    ))
    .style(move |style| settings_card_style(style, palette).gap(10.0));

    scroll(
        v_stack((
            page_title(i18n::Key::Updates, palette),
            spacer(7.0),
            page_description(i18n::Key::UpdatesDescription, palette),
            spacer(28.0),
            status_card,
            spacer(20.0),
            notes_card,
            spacer(20.0),
            automatic_card,
        ))
        .style(|style| {
            rtl_column(style)
                .width_full()
                .padding_horiz(SETTINGS_PAGE_INSET_PX)
                .padding_vert(38.0)
        }),
    )
    .style(move |style| {
        style
            .min_width(0.0)
            .height_full()
            .flex_grow(1.0)
            .background(palette.canvas)
    })
}

fn status_text(stage: &Stage) -> UiText {
    let message = match stage {
        Stage::Idle => return UiText::default(),
        Stage::Checking => msg!(UpdateChecking),
        Stage::UpToDate => msg!(UpdateUpToDate),
        Stage::Available(release) => {
            msg!(UpdateAvailable, "version" => release.version.to_string())
        }
        Stage::Held(release) => msg!(UpdateHeld, "version" => release.version.to_string()),
        Stage::Unpackaged(release) => {
            msg!(UpdateUnpackaged, "version" => release.version.to_string())
        }
        Stage::Downloading {
            received, total, ..
        } => match total {
            Some(total) if *total > 0 => {
                msg!(UpdateDownloadingPercent, "percent" => received * 100 / total)
            }
            _ => msg!(UpdateDownloading),
        },
        Stage::Installed(version) => {
            msg!(UpdateInstalledRestart, "version" => version.to_string())
        }
        Stage::Failed(error) | Stage::Unsupported(error) => failure(error),
    };
    message.into()
}

fn failure(error: &UpdateError) -> i18n::Message {
    match error {
        UpdateError::NotInstalled => msg!(UpdateNotInstalled),
        UpdateError::ReadOnly => msg!(UpdateReadOnly),
        UpdateError::Network => msg!(UpdateNetworkFailed),
        UpdateError::RateLimited => msg!(UpdateRateLimited),
        UpdateError::Response => msg!(UpdateResponseFailed),
        UpdateError::NoPackage => msg!(UpdateNoPackage),
        UpdateError::Checksum => msg!(UpdateChecksumFailed),
        UpdateError::Package(detail) => msg!(UpdateFailed, "error" => (*detail).to_owned()),
        UpdateError::Io(detail) => msg!(UpdateFailed, "error" => detail.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_projection_does_not_invalidate_the_window() {
        let scope = floem::reactive::Scope::new();
        floem::reactive::with_scope(scope, || {
            let global = Rc::new(RefCell::new(GlobalApplication::load(None).store));
            let updates = Updates::new(global, Rc::new(|_| Ok(false)));
            let notifications = Rc::new(Cell::new(0));
            let count = notifications.clone();
            let prompt = updates.prompt;
            let automatic = updates.automatic;
            let stage = updates.stage;
            create_effect(move |_| {
                prompt.get();
                automatic.get();
                stage.get();
                count.set(count.get() + 1);
            });
            let initial = notifications.get();
            updates.project();
            updates.project();
            assert_eq!(notifications.get(), initial);
        });
        scope.dispose();
    }

    #[test]
    fn installed_update_requires_a_click_and_retains_restart_after_errors_or_later() {
        let scope = floem::reactive::Scope::new();
        floem::reactive::with_scope(scope, || {
            let root = crate::test_support::workspace("stillus-update-restart");
            std::fs::write(root.join("stillus"), b"fixture").unwrap();
            std::fs::write(root.join("build.json"), b"{}").unwrap();
            let global = Rc::new(RefCell::new(GlobalApplication::load(Some(&root)).store));
            let calls = Rc::new(Cell::new(0));
            let invoked = calls.clone();
            let mut updates = Updates::new(
                global,
                Rc::new(move |_| {
                    invoked.set(invoked.get() + 1);
                    Err(msg!(ResolveSaveFirst).into())
                }),
            );
            updates.installation =
                Rc::new(Some(Installation::linux(root.join("stillus")).unwrap()));
            updates.request_restart();
            assert_eq!(calls.get(), 0);
            let transition = application::updates::apply(
                Stage::Idle,
                application::updates::Message::Installed(Ok(Version::new(9, 9, 9))),
                false,
                None,
            );
            updates.global.borrow_mut().update.stage = transition.stage;
            updates.global.borrow_mut().update.prompt = transition.show_prompt;
            updates.project();
            assert!(updates.prompt.get_untracked());
            assert_eq!(calls.get(), 0);
            updates.check(CheckMode::Manual);
            assert_eq!(
                updates.stage.get_untracked(),
                Stage::Installed(Version::new(9, 9, 9))
            );
            updates.request_restart();
            assert_eq!(calls.get(), 1);
            assert!(updates.restart_error.get_untracked().is_some());
            assert!(!updates.restarting.get_untracked());
            updates.dismiss();
            assert!(!updates.prompt.get_untracked());
            assert!(updates.global.borrow().updates().dismissed.is_none());
            updates.request_restart();
            assert_eq!(calls.get(), 2);
            assert!(matches!(updates.stage.get_untracked(), Stage::Installed(_)));
            std::fs::remove_dir_all(root).unwrap();
        });
        scope.dispose();
    }
}
