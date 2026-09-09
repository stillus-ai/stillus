// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

#[derive(Clone, Copy)]
pub(crate) enum IconButtonTone {
    Secondary,
    Primary,
    Danger,
    Status,
    /// Quiet control on the dark sidebar surface.
    Sidebar,
}

#[derive(Clone, Copy)]
pub(crate) struct ButtonColors {
    background: Color,
    foreground: Color,
    border: Color,
    hover: Color,
    hover_foreground: Color,
}

pub(crate) fn button_colors(tone: IconButtonTone, palette: Palette) -> ButtonColors {
    match tone {
        IconButtonTone::Secondary => ButtonColors {
            background: palette.paper,
            foreground: palette.ink,
            border: palette.divider,
            hover: palette.accent_soft,
            hover_foreground: palette.accent,
        },
        IconButtonTone::Primary => ButtonColors {
            background: palette.accent,
            foreground: Color::WHITE,
            border: palette.accent,
            hover: Color::rgb8(44, 82, 117),
            hover_foreground: Color::WHITE,
        },
        IconButtonTone::Danger => ButtonColors {
            background: palette.paper,
            foreground: palette.danger,
            border: Color::rgb8(232, 205, 205),
            hover: Color::rgb8(250, 235, 235),
            hover_foreground: palette.danger,
        },
        IconButtonTone::Status => ButtonColors {
            background: palette.paper,
            foreground: palette.accent,
            border: palette.divider,
            hover: palette.accent_soft,
            hover_foreground: palette.accent,
        },
        IconButtonTone::Sidebar => ButtonColors {
            background: palette.sidebar_active,
            foreground: palette.sidebar_ink,
            border: palette.sidebar_border,
            hover: Color::rgb8(72, 83, 97),
            hover_foreground: palette.sidebar_ink,
        },
    }
}

pub(crate) const BUTTON_SIZE_PX: f64 = 32.0;
pub(crate) const STATUS_BUTTON_SIZE_PX: f64 = 28.0;
pub(crate) const PASSWORD_DIALOG_SECONDARY_BUTTON_WIDTH_PX: f64 = 84.0;
pub(crate) const PASSWORD_DIALOG_PRIMARY_BUTTON_WIDTH_PX: f64 = 134.0;

/// Semantic actions, independent of translated text. Custom actions retain a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonAction {
    Copy,
    Cut,
    Paste,
    Send,
    Stop,
    Refresh,
    Retry,
    Save,
    Cancel,
    Close,
    Add,
    Delete,
    Edit,
    Back,
    Search,
    Download,
    Settings,
    Pin,
    Favorite,
    Custom(&'static str),
}
#[cfg(test)]
const STANDARD_ACTIONS: &[ButtonAction] = &[
    ButtonAction::Copy,
    ButtonAction::Cut,
    ButtonAction::Paste,
    ButtonAction::Send,
    ButtonAction::Stop,
    ButtonAction::Refresh,
    ButtonAction::Retry,
    ButtonAction::Save,
    ButtonAction::Cancel,
    ButtonAction::Close,
    ButtonAction::Add,
    ButtonAction::Delete,
    ButtonAction::Edit,
    ButtonAction::Back,
    ButtonAction::Search,
    ButtonAction::Download,
    ButtonAction::Settings,
    ButtonAction::Pin,
    ButtonAction::Favorite,
];
impl ButtonAction {
    pub(crate) const fn icon(self) -> &'static str {
        match self {
            Self::Copy => ICON_COPY,
            Self::Cut => ICON_CUT,
            Self::Paste => ICON_PASTE,
            Self::Send => ICON_SEND,
            Self::Stop => ICON_STOP,
            Self::Refresh | Self::Retry => ICON_RETRY,
            Self::Save => ICON_SAVE,
            Self::Cancel | Self::Close => ICON_CANCEL,
            Self::Add => ICON_CREATE,
            Self::Delete => ICON_TRASH,
            Self::Edit => ICON_RENAME,
            Self::Back => ICON_BACK,
            Self::Search => ICON_SEARCH,
            Self::Download => ICON_DOWNLOAD,
            Self::Settings => ICON_SETTINGS,
            Self::Pin => ICON_PIN,
            Self::Favorite => ICON_STAR,
            Self::Custom(icon) => icon,
        }
    }
    pub(super) fn show_label(self, context: ButtonContext) -> bool {
        matches!(context, ButtonContext::Dialog | ButtonContext::Menu)
            || matches!(self, Self::Custom(_))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonContext {
    Control,
    Dialog,
    Menu,
}

// Icon-only buttons retain a title even when unavailable.
// Hover and keyboard focus share the same reactive content.
thread_local! {
    static BUTTON_FOCUS_TARGETS: RefCell<std::collections::HashMap<ViewId, ViewId>> = RefCell::new(std::collections::HashMap::new());
    static POINTER_FOCUS_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn button_focus_is_pointer() -> bool {
    POINTER_FOCUS_RESTORE.with(|pointer| pointer.get())
}

pub(crate) fn button_request_pointer_focus(id: ViewId) {
    POINTER_FOCUS_RESTORE.with(|pointer| pointer.set(true));
    id.request_focus();
    exec_after(Duration::from_millis(50), |_| {
        POINTER_FOCUS_RESTORE.with(|pointer| pointer.set(false));
    });
}

pub(crate) fn button_focus_target(id: ViewId) -> ViewId {
    BUTTON_FOCUS_TARGETS.with(|targets| targets.borrow().get(&id).copied().unwrap_or(id))
}

fn titled_button(
    child: impl IntoView + 'static,
    title: Rc<dyn Fn() -> String>,
    palette: Palette,
) -> floem::views::Tooltip {
    let child = child.into_view();
    let focus_target = child.id();
    let origin = Rc::new(std::cell::Cell::new(Point::ZERO));
    let moved = origin.clone();
    let overlay = Rc::new(RefCell::new(None));
    let opened = overlay.clone();
    let focus_title = title.clone();
    let pointer_focus = Rc::new(std::cell::Cell::new(false));
    let pointer_down = pointer_focus.clone();
    let pointer_overlay = overlay.clone();
    let focused = child
        .on_event_cont(EventListener::PointerDown, move |_| {
            pointer_down.set(true);
            close_tooltip(&pointer_overlay);
            let reset = pointer_down.clone();
            exec_after(Duration::from_millis(50), move |_| reset.set(false));
        })
        .on_move(move |point| moved.set(point))
        .on_event_cont(EventListener::FocusGained, move |_| {
            if !pointer_focus.get() && !button_focus_is_pointer() && opened.borrow().is_none() {
                let title = focus_title.clone();
                let id = add_overlay(origin.get() + (0.0, BUTTON_SIZE_PX + 6.0), move |_| {
                    tooltip_content(title.clone(), palette).pointer_events(|| false)
                });
                *opened.borrow_mut() = Some(id);
            }
        });
    let lost = overlay.clone();
    let inactive = overlay.clone();
    let tooltip = focused
        .on_event_cont(EventListener::FocusLost, move |_| close_tooltip(&lost))
        .on_event_cont(EventListener::WindowLostFocus, move |_| {
            close_tooltip(&inactive)
        })
        .on_cleanup(move || close_tooltip(&overlay))
        .tooltip(move || tooltip_content(title.clone(), palette).pointer_events(|| false));
    let tooltip_id = tooltip.id();
    BUTTON_FOCUS_TARGETS.with(|targets| targets.borrow_mut().insert(tooltip_id, focus_target));
    tooltip.on_cleanup(move || {
        BUTTON_FOCUS_TARGETS.with(|targets| targets.borrow_mut().remove(&tooltip_id));
    })
}
fn close_tooltip(overlay: &RefCell<Option<ViewId>>) {
    if let Some(id) = overlay.borrow_mut().take() {
        remove_overlay(id);
    }
}
fn tooltip_content(title: Rc<dyn Fn() -> String>, palette: Palette) -> impl IntoView {
    label(move || {
        let title = title();
        assert!(!title.trim().is_empty(), "button title must not be empty");
        title
    })
    .style(move |s| tooltip_style(s, palette))
}
pub(crate) fn tooltip_label(title: String, palette: Palette) -> impl IntoView {
    text(title).style(move |s| tooltip_style(s, palette))
}
fn tooltip_style(s: Style, palette: Palette) -> Style {
    s.padding_vert(6.0)
        .padding_horiz(9.0)
        .background(Color::rgb8(28, 33, 40))
        .color(palette.sidebar_ink)
        .font_family(UI_FONT_FAMILY.to_owned())
        .font_size(12.0)
        .border(1.0)
        .border_color(Color::rgb8(58, 66, 77))
        .border_radius(5.0)
}

struct ButtonStyle {
    labeled: bool,
    tone: IconButtonTone,
    palette: Palette,
    compact_size: Option<f64>,
}
fn button(
    icon: impl Fn() -> &'static str + 'static,
    title: impl Fn() -> String + 'static,
    style: ButtonStyle,
    enabled: impl Fn() -> bool + 'static,
    active: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    let ButtonStyle {
        labeled,
        tone,
        palette,
        compact_size,
    } = style;
    let title: Rc<dyn Fn() -> String> = Rc::new(title);
    let caption = title.clone();
    // Keep application predicates in their own reactive scope. Style updates
    // and pointer callbacks must not retrack the application's revision signal.
    let enabled = floem::reactive::create_memo(move |_| enabled());
    let press_enabled = enabled;
    let colors = button_colors(tone, palette);
    let size = compact_size.unwrap_or(if matches!(tone, IconButtonTone::Status) {
        STATUS_BUTTON_SIZE_PX
    } else {
        BUTTON_SIZE_PX
    });
    let glyph = svg(icon())
        .update_value(move || {
            let icon = icon();
            if icon == ICON_BACK && i18n::current().is_rtl() {
                ICON_CHEVRON_RIGHT
            } else {
                icon
            }
        })
        .style(|s| s.size(16.0, 16.0).flex_shrink(0.0));
    let content = h_stack((
        glyph,
        label(move || caption()).style(move |s| {
            s.font_size(13.0)
                .selectable(false)
                .apply_if(!labeled, |s| s.hide())
        }),
    ))
    .style(|s| rtl_row(s).items_center().gap(7.0));
    let surface = reliable_button(content, move || {
        if press_enabled.get_untracked() {
            action();
        }
    })
    .style(move |s| {
        let available = enabled.get();
        let selected = active();
        s.height(size)
            .min_width(size)
            .flex_shrink(0.0)
            .items_center()
            .justify_center()
            .apply_if(labeled, |s| s.padding_horiz(14.0))
            .apply_if(!labeled, |s| s.width(size))
            .cursor(if available {
                CursorStyle::Pointer
            } else {
                CursorStyle::Default
            })
            .background(if !available {
                palette.canvas
            } else if selected {
                palette.accent_soft
            } else {
                colors.background
            })
            .color(if !available {
                if labeled {
                    palette.muted
                } else {
                    palette.divider
                }
            } else if selected {
                palette.accent
            } else {
                colors.foreground
            })
            .border(1.0)
            .border_color(if selected {
                palette.accent
            } else {
                colors.border
            })
            .border_radius(5.0)
            .hover(move |s| s.background(colors.hover).color(colors.hover_foreground))
            .focus(|s| s.border_color(palette.accent))
    });
    if labeled {
        surface.into_any()
    } else {
        titled_button(surface, title, palette).into_any()
    }
}
pub(crate) fn icon_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    tone: IconButtonTone,
    palette: Palette,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || icon,
        title,
        ButtonStyle {
            labeled: false,
            tone,
            palette,
            compact_size: None,
        },
        || true,
        || false,
        action,
    )
}
pub(crate) fn enabled_icon_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    tone: IconButtonTone,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || icon,
        title,
        ButtonStyle {
            labeled: false,
            tone,
            palette,
            compact_size: None,
        },
        enabled,
        || false,
        action,
    )
}
pub(crate) fn icon_toggle_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    palette: Palette,
    active: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || icon,
        title,
        ButtonStyle {
            labeled: false,
            tone: IconButtonTone::Secondary,
            palette,
            compact_size: None,
        },
        || true,
        active,
        action,
    )
}
pub(crate) fn enabled_icon_toggle_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    active: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || icon,
        title,
        ButtonStyle {
            labeled: false,
            tone: IconButtonTone::Secondary,
            palette,
            compact_size: None,
        },
        enabled,
        active,
        action,
    )
}

/// A pending operation retains its hover hint while preventing duplicate work.
pub(crate) fn busy_icon_toggle_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    active: impl Fn() -> bool + 'static,
    busy: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    let busy: Rc<dyn Fn() -> bool> = Rc::new(busy);
    let busy_icon = busy.clone();
    let busy_title = busy.clone();
    button(
        move || {
            if busy_icon() {
                ButtonAction::Refresh.icon()
            } else {
                icon
            }
        },
        move || {
            if busy_title() {
                tr!(WaitingAutosave)
            } else {
                title()
            }
        },
        ButtonStyle {
            labeled: false,
            tone: IconButtonTone::Secondary,
            palette,
            compact_size: None,
        },
        move || enabled() && !busy(),
        active,
        action,
    )
}

pub(crate) fn sidebar_sort_button(
    hovered: RwSignal<bool>,
    palette: Palette,
    action: impl Fn() + 'static,
) -> AnyView {
    let control =
        reliable_button(svg(ICON_SORT).style(|s| s.size(14.0, 14.0)), action).style(move |s| {
            let visible = hovered.get();
            s.size(24.0, 24.0)
                .items_center()
                .justify_center()
                .background(Color::TRANSPARENT)
                .color(if visible {
                    palette.sidebar_muted
                } else {
                    Color::TRANSPARENT
                })
                .border(1.0)
                .border_color(Color::TRANSPARENT)
                .border_radius(5.0)
                .hover(move |s| {
                    s.color(if visible {
                        palette.sidebar_ink
                    } else {
                        Color::TRANSPARENT
                    })
                })
        });
    titled_button(control, Rc::new(|| tr!(SortNotes)), palette).into_any()
}
pub(crate) fn action_button(
    kind: ButtonAction,
    title: impl Fn() -> String + 'static,
    tone: IconButtonTone,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || kind.icon(),
        title,
        ButtonStyle {
            labeled: kind.show_label(ButtonContext::Control),
            tone,
            palette,
            compact_size: None,
        },
        enabled,
        || false,
        action,
    )
}
pub(crate) fn dialog_action_button(
    kind: ButtonAction,
    title: impl Fn() -> String + 'static,
    tone: IconButtonTone,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || kind.icon(),
        title,
        ButtonStyle {
            labeled: true,
            tone,
            palette,
            compact_size: None,
        },
        enabled,
        || false,
        action,
    )
}

pub(crate) fn dialog_button(
    kind: ButtonAction,
    title: i18n::Message,
    tone: IconButtonTone,
    palette: Palette,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        move || kind.icon(),
        move || title.to_string(),
        ButtonStyle {
            labeled: kind.show_label(ButtonContext::Dialog),
            tone,
            palette,
            compact_size: None,
        },
        || true,
        || false,
        action,
    )
}
pub(crate) fn password_dialog_button(
    kind: ButtonAction,
    label_text: i18n::Message,
    tone: IconButtonTone,
    palette: Palette,
    disabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> impl IntoView {
    let colors = button_colors(tone, palette);
    let button_width = match tone {
        IconButtonTone::Primary => PASSWORD_DIALOG_PRIMARY_BUTTON_WIDTH_PX,
        _ => PASSWORD_DIALOG_SECONDARY_BUTTON_WIDTH_PX,
    };
    let disabled: Rc<dyn Fn() -> bool> = Rc::new(disabled);
    let action: Rc<dyn Fn()> = Rc::new(action);
    let active_background = match tone {
        IconButtonTone::Primary => Color::rgb8(35, 72, 105),
        IconButtonTone::Danger => Color::rgb8(244, 220, 220),
        IconButtonTone::Sidebar => Color::rgb8(82, 94, 110),
        IconButtonTone::Secondary | IconButtonTone::Status => palette.divider,
    };
    let disabled_background = match tone {
        IconButtonTone::Primary => Color::rgb8(166, 184, 200),
        _ => palette.canvas,
    };
    let trigger_disabled = disabled.clone();
    let trigger: Rc<dyn Fn()> = Rc::new(move || {
        if trigger_disabled() {
            return;
        }
        action();
    });
    let pointer_trigger = trigger.clone();
    let keyboard_trigger = trigger;
    let view_disabled = disabled;
    let surface = PrimaryPointerView::new(empty(), move |_| pointer_trigger())
        .capture_pointer()
        .keyboard_navigable()
        .on_event(EventListener::KeyDown, move |event| {
            if is_keyboard_activation(event) {
                keyboard_trigger();
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .style(move |style| {
            style
                .size_full()
                .items_center()
                .justify_center()
                .cursor(CursorStyle::Pointer)
                .background(colors.background)
                .border(1.0)
                .border_color(colors.border)
                .border_radius(5.0)
                .hover(move |style| {
                    if matches!(tone, IconButtonTone::Primary) {
                        style.border_color(colors.hover)
                    } else {
                        style.background(colors.hover)
                    }
                })
                .disabled(move |style| {
                    style.background(disabled_background).border_color(
                        if matches!(tone, IconButtonTone::Primary) {
                            disabled_background
                        } else {
                            palette.divider
                        },
                    )
                })
                .active(move |style| {
                    style
                        .background(active_background)
                        .border_color(active_background)
                })
        })
        .disabled(move || view_disabled());
    let label = text(label_text)
        .pointer_events(|| false)
        .style(move |style| {
            style
                .font_family(UI_FONT_FAMILY.to_owned())
                .font_size(13.0)
                .color(colors.foreground)
                .selectable(false)
        });
    let label = h_stack((
        svg(kind.icon()).style(|s| s.size(16.0, 16.0).flex_shrink(0.0)),
        label,
    ))
    .pointer_events(|| false)
    .style(move |style| {
        rtl_row(style)
            .absolute()
            .size_full()
            .items_center()
            .justify_center()
            .gap(7.0)
            .color(colors.foreground)
    });
    stack((surface, label)).style(move |style| {
        style
            .font_family(UI_FONT_FAMILY.to_owned())
            .font_size(13.0)
            .width(button_width)
            .min_width(button_width)
            .max_width(button_width)
            .height(BUTTON_SIZE_PX)
            .min_height(BUTTON_SIZE_PX)
            .max_height(BUTTON_SIZE_PX)
            .flex_shrink(0.0)
    })
}

/// A custom action whose caption can wrap or include structured content.
pub(crate) fn content_button(
    icon: &'static str,
    content: impl IntoView + 'static,
    action: impl Fn() + 'static,
) -> impl IntoView {
    reliable_button(
        h_stack((
            svg(icon).style(|s| s.size(16.0, 16.0).flex_shrink(0.0)),
            content,
        ))
        .style(|s| rtl_row(s).items_center().gap(7.0)),
        action,
    )
}

pub(crate) fn compact_icon_button(
    icon: impl Fn() -> &'static str + 'static,
    title: impl Fn() -> String + 'static,
    tone: IconButtonTone,
    palette: Palette,
    size: f64,
    enabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> AnyView {
    button(
        icon,
        title,
        ButtonStyle {
            labeled: false,
            tone,
            palette,
            compact_size: Some(size),
        },
        enabled,
        || false,
        action,
    )
}

pub(super) fn menu_button(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    action: impl Fn() + 'static,
) -> impl IntoView {
    let labeled = ButtonAction::Custom(icon).show_label(ButtonContext::Menu);
    let title: Rc<dyn Fn() -> String> = Rc::new(title);
    let caption = title.clone();
    let enabled = Rc::new(enabled);
    let can_press = enabled.clone();
    reliable_button(
        h_stack((
            svg(icon).style(|s| s.size(15.0, 15.0)),
            label(move || caption()).style(move |s| {
                s.font_size(13.0)
                    .selectable(false)
                    .apply_if(!labeled, |s| s.hide())
            }),
        ))
        .style(|s| rtl_row(s).width_full().items_center().gap(8.0)),
        move || {
            if can_press() {
                action();
            }
        },
    )
    .style(move |s| {
        s.width_full()
            .height(32.0)
            .padding_horiz(8.0)
            .items_center()
            .border_radius(5.0)
            .color(if enabled() {
                palette.ink
            } else {
                palette.muted
            })
            .hover(|s| s.background(palette.canvas))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_actions_have_icons_and_labels_only_in_menus_and_dialogs() {
        for &action in STANDARD_ACTIONS {
            assert!(action.icon().contains("<svg"));
            assert!(!action.show_label(ButtonContext::Control));
            assert!(action.show_label(ButtonContext::Dialog));
            assert!(action.show_label(ButtonContext::Menu));
        }
        assert!(ButtonAction::Custom(ICON_LOCK).show_label(ButtonContext::Control));
    }
}
