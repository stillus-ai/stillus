// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) const SETTINGS_PAGE_INSET_PX: f64 = 44.0;

pub(crate) const FONT_SCREEN: f64 = 24.0;
pub(crate) const FONT_SECTION: f64 = 17.0;
pub(crate) const FONT_CARD: f64 = 15.0;
pub(crate) const FONT_BODY: f64 = 14.0;
pub(crate) const FONT_CAPTION: f64 = 12.0;
pub(crate) const MONO_FONT_FAMILY: &str = "Noto Sans Mono";
pub(crate) const HEADING_FONT_FAMILY: &str = "Noto Serif";

/// Register bundled faces before shaping text or measuring editor advances.
pub(crate) fn register_fonts() {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        let faces: &[&[u8]] = &[
            include_bytes!("../../assets/fonts/NotoSans-Regular.ttf"),
            include_bytes!("../../assets/fonts/NotoSans-Italic.ttf"),
            include_bytes!("../../assets/fonts/NotoSans-SemiBold.ttf"),
            include_bytes!("../../assets/fonts/NotoSans-SemiBoldItalic.ttf"),
            include_bytes!("../../assets/fonts/NotoSansMono-Regular.ttf"),
            include_bytes!("../../assets/fonts/NotoSerif-SemiBold.ttf"),
        ];
        let mut fonts = floem::text::FONT_SYSTEM.lock();
        for face in faces {
            fonts.db_mut().load_font_data(face.to_vec());
        }
    });
}

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) canvas: Color,
    pub(crate) sidebar: Color,
    pub(crate) sidebar_active: Color,
    pub(crate) sidebar_ink: Color,
    pub(crate) sidebar_muted: Color,
    pub(crate) sidebar_border: Color,
    pub(crate) sidebar_accent: Color,
    pub(crate) paper: Color,
    pub(crate) ink: Color,
    pub(crate) ink2: Color,
    pub(crate) ink3: Color,
    pub(crate) muted: Color,
    pub(crate) divider: Color,
    pub(crate) accent: Color,
    pub(crate) accent_soft: Color,
    pub(crate) danger: Color,
    pub(crate) scrollbar: Color,
}

impl Palette {
    pub(crate) fn new() -> Self {
        Self {
            canvas: Color::rgb8(246, 247, 248),
            sidebar: Color::rgb8(36, 42, 51),
            sidebar_active: Color::rgb8(57, 66, 78),
            sidebar_ink: Color::rgb8(244, 246, 248),
            sidebar_muted: Color::rgb8(164, 173, 184),
            sidebar_border: Color::rgb8(58, 66, 77),
            sidebar_accent: Color::rgb8(143, 184, 220),
            paper: Color::rgb8(255, 255, 255),
            ink: Color::rgb8(35, 39, 45),
            ink2: Color::rgb8(97, 104, 112),
            ink3: Color::rgb8(130, 137, 145),
            muted: Color::rgb8(97, 104, 112),
            divider: Color::rgb8(226, 229, 233),
            accent: Color::rgb8(54, 94, 130),
            accent_soft: Color::rgb8(229, 238, 246),
            danger: Color::rgb8(164, 69, 69),
            scrollbar: Color::rgba8(35, 39, 45, 96),
        }
    }
}

pub(crate) fn text_input_affordance(
    style: Style,
    placeholder_color: Color,
    caret_color: Color,
) -> Style {
    style
        .cursor(CursorStyle::Text)
        .cursor_color(floem::peniko::Brush::Solid(caret_color))
        .class(PlaceholderTextClass, move |style| {
            style.color(placeholder_color)
        })
}

/// One field affordance for every engine form: the creation popover and the
/// toolbar editing bars share height, radius, colors and focus ring.
pub(crate) fn form_field_style(style: Style, palette: Palette, invalid: bool) -> Style {
    text_input_affordance(style, palette.ink3, palette.accent)
        .height(FORM_FIELD_HEIGHT_PX)
        .items_center()
        .padding_horiz(10.0)
        .background(palette.canvas)
        .color(palette.ink)
        .border(1.0)
        .border_color(if invalid {
            palette.danger
        } else {
            palette.divider
        })
        .border_radius(6.0)
        .font_size(FONT_BODY)
        .focus(move |style| {
            if invalid {
                style
            } else {
                style.background(palette.paper).border_color(palette.accent)
            }
        })
}

/// One card for every settings page: the general page, encryption and both AI
/// sections share width, padding, surface and border. Callers add their own
/// gap because a card of stacked fields and a card of prose need different
/// rhythms.
pub(crate) fn settings_card_style(style: Style, palette: Palette) -> Style {
    rtl_column(style)
        .width_full()
        .min_width(0.0)
        .max_width(SETTINGS_CARD_MAX_WIDTH_PX)
        .padding(SETTINGS_CARD_PADDING_PX)
        .background(palette.paper)
        .border(1.0)
        .border_color(palette.divider)
        .border_radius(8.0)
}

/// The shape every settings form control shares: text fields, the masked
/// secret fields and the AI model dropdown are one affordance, so a settings
/// form never mixes control heights or radii.
pub(crate) fn settings_control_style(style: Style, palette: Palette) -> Style {
    style
        .width_full()
        .min_width(0.0)
        .height(SETTINGS_FIELD_HEIGHT_PX)
        .items_center()
        .padding_horiz(12.0)
        .background(palette.paper)
        .color(palette.ink)
        .border(1.0)
        .border_color(palette.divider)
        .border_radius(6.0)
        .font_size(FONT_BODY)
}

/// A settings text field: the shared control plus the placeholder and caret
/// colors and the focused border every other field in the app already has.
pub(crate) fn settings_input_style(style: Style, palette: Palette) -> Style {
    text_input_affordance(
        settings_control_style(style, palette),
        palette.ink3,
        palette.accent,
    )
    .focus(move |style| style.border_color(palette.accent))
}

/// A settings field that never reveals what it holds: the master password
/// fields and the AI API key. An empty field prints its placeholder in the
/// placeholder color instead of the value color.
pub(crate) fn settings_secret_style(
    style: Style,
    palette: Palette,
    empty: bool,
    active: bool,
) -> Style {
    settings_control_style(style, palette)
        .cursor(CursorStyle::Text)
        .font_size(FONT_BODY)
        .color(if empty { palette.ink3 } else { palette.ink })
        .border_color(if active {
            palette.accent
        } else {
            palette.divider
        })
}

/// The disabled affordance shared by every settings control: a control that
/// cannot act says so instead of looking pressable.
pub(crate) fn disabled_control_style(style: Style, palette: Palette) -> Style {
    style.disabled(move |style| {
        style
            .cursor(CursorStyle::Default)
            .background(palette.canvas)
            .color(palette.muted)
            .border_color(palette.divider)
    })
}

/// The small caption above a settings control ("Path", "API key", "Model").
pub(crate) fn settings_field_label(key: i18n::Key, palette: Palette) -> impl IntoView {
    caption(move || key.to_string(), palette.ink2)
}

/// A settings paragraph: card subtitles, form hints and inline explanations.
pub(crate) fn settings_hint(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .width_full()
            .font_size(FONT_BODY)
            .line_height(1.4)
            .color(palette.muted)
            .selectable(false)
    })
}

pub(crate) fn rtl_row(style: Style) -> Style {
    style.flex_direction(if i18n::current().is_rtl() {
        floem::taffy::FlexDirection::RowReverse
    } else {
        floem::taffy::FlexDirection::Row
    })
}

pub(crate) fn rtl_column(style: Style) -> Style {
    if i18n::current().is_rtl() {
        style.items_end()
    } else {
        style
    }
}

pub(crate) const FORM_FIELD_HEIGHT_PX: f64 = 32.0;
pub(crate) const SETTINGS_CARD_MAX_WIDTH_PX: f64 = 720.0;
pub(crate) const SETTINGS_CARD_PADDING_PX: f64 = 22.0;
pub(crate) const SETTINGS_FIELD_HEIGHT_PX: f64 = 40.0;
pub(crate) const UI_FONT_FAMILY: &str = "Noto Sans";

pub(crate) fn modal_backdrop(style: Style) -> Style {
    style
        .absolute()
        .size_full()
        .items_center()
        .justify_center()
        .background(Color::rgba8(24, 29, 36, 92))
}

pub(crate) fn dialog_card_style(style: Style, palette: Palette, width: f64, padding: f64) -> Style {
    style
        .width(width)
        .padding(padding)
        .background(palette.paper)
        .color(palette.ink)
        .border(1.0)
        .border_color(palette.divider)
        .border_radius(9.0)
}
