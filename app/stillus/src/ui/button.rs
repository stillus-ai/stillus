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

/// Semantic actions, independent of translated text. Presentation depends on context.
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
        matches!(context, ButtonContext::Form) || matches!(self, Self::Custom(_))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonContext {
    Toolbar,
    Form,
}

// Icon-only buttons retain a title even when unavailable.
// Hover and keyboard focus share the same reactive content.
thread_local! {
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
    id
}

fn titled_button(
    child: impl IntoView + 'static,
    title: Rc<dyn Fn() -> String>,
    palette: Palette,
) -> AnyView {
    anchored_tooltip(
        child,
        Rc::new(move || {
            let title = title();
            assert!(!title.trim().is_empty(), "button title must not be empty");
            title
        }),
        palette,
    )
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
            s.font_size(crate::ui::FONT_BODY as f32)
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

/// Keep a row action's entire surface hidden until hover or keyboard focus.
pub(crate) fn sidebar_close_button(
    hovered: RwSignal<bool>,
    palette: Palette,
    action: impl Fn() + 'static,
) -> AnyView {
    let control = reliable_button(
        svg(ButtonAction::Close.icon()).style(|s| s.size(14.0, 14.0)),
        action,
    )
    .style(move |s| {
        s.size(22.0, 22.0)
            .flex_shrink(0.0)
            .items_center()
            .justify_center()
            .background(Color::TRANSPARENT)
            .color(if hovered.get() {
                palette.sidebar_muted
            } else {
                Color::TRANSPARENT
            })
            .border(1.0)
            .border_color(Color::TRANSPARENT)
            .border_radius(4.0)
            .hover(move |s| {
                s.color(palette.sidebar_ink)
                    .background(palette.sidebar_active)
            })
            .focus_visible(move |s| {
                s.color(palette.sidebar_ink)
                    .background(palette.sidebar_active)
                    .border_color(palette.accent)
            })
    });
    titled_button(control, Rc::new(|| tr!(RemoveSidebar)), palette).into_any()
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
/// A message action reserves its space and reveals itself on header hover or
/// keyboard focus, so appearing controls never move the message text.
pub(crate) fn hover_copy_button(
    hovered: RwSignal<bool>,
    palette: Palette,
    action: impl Fn() + 'static,
) -> AnyView {
    let colors = button_colors(IconButtonTone::Secondary, palette);
    let control =
        reliable_button(svg(ICON_COPY).style(|s| s.size(16.0, 16.0)), action).style(move |s| {
            let visible = hovered.get();
            s.size(BUTTON_SIZE_PX, BUTTON_SIZE_PX)
                .flex_shrink(0.0)
                .items_center()
                .justify_center()
                .background(if visible {
                    colors.background
                } else {
                    Color::TRANSPARENT
                })
                .color(if visible {
                    colors.foreground
                } else {
                    Color::TRANSPARENT
                })
                .border(1.0)
                .border_color(if visible {
                    colors.border
                } else {
                    Color::TRANSPARENT
                })
                .border_radius(5.0)
                .hover(move |s| s.background(colors.hover).color(colors.hover_foreground))
                .focus_visible(move |s| {
                    s.background(colors.background)
                        .color(colors.foreground)
                        .border_color(palette.accent)
                })
        });
    titled_button(control, Rc::new(|| tr!(Copy)), palette).into_any()
}
/// Toolbar actions show a caption only when their meaning needs explanation.
pub(crate) fn toolbar_action_button(
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
            labeled: kind.show_label(ButtonContext::Toolbar),
            tone,
            palette,
            compact_size: None,
        },
        enabled,
        || false,
        action,
    )
}
/// Form actions always show both an icon and a localized action caption.
pub(crate) fn form_action_button(
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
            labeled: kind.show_label(ButtonContext::Form),
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
            labeled: kind.show_label(ButtonContext::Form),
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
    let minimum = match tone {
        IconButtonTone::Primary => PASSWORD_DIALOG_PRIMARY_BUTTON_WIDTH_PX,
        _ => PASSWORD_DIALOG_SECONDARY_BUTTON_WIDTH_PX,
    };
    let disabled = floem::reactive::create_memo(move |_| disabled());
    let colors = button_colors(tone, palette);
    form_action_button(
        kind,
        move || label_text.to_string(),
        tone,
        palette,
        move || !disabled.get(),
        action,
    )
    .style(move |s| {
        s.min_width(minimum)
            .padding_horiz(8.0)
            .color(colors.foreground)
            .apply_if(disabled.get(), |s| {
                s.background(match tone {
                    IconButtonTone::Primary => Color::rgb8(166, 184, 200),
                    _ => palette.canvas,
                })
            })
            .focus(|s| s.outline(1.0).outline_color(palette.accent))
    })
    .disabled(move || disabled.get())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms_label_every_action_and_toolbars_label_custom_actions() {
        for &action in STANDARD_ACTIONS {
            assert!(action.icon().contains("<svg"));
            assert!(!action.show_label(ButtonContext::Toolbar));
            assert!(action.show_label(ButtonContext::Form));
        }
        for context in [ButtonContext::Toolbar, ButtonContext::Form] {
            assert!(ButtonAction::Custom(ICON_LOCK).show_label(context));
        }
    }
}
