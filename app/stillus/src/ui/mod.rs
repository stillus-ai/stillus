// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Shared native controls. Screens supply values and callbacks; controls own interaction.
use crate::{
    i18n::{self, tr},
    text,
};
use floem::{
    AnyView, View, ViewId,
    action::{add_overlay, exec_after, remove_overlay},
    event::{Event, EventListener, EventPropagation},
    keyboard::{Key, NamedKey},
    kurbo::Point,
    pointer::PointerInputEvent,
    prelude::*,
    reactive::create_effect,
    style::{CursorStyle, Style},
};
use std::{cell::RefCell, rc::Rc, time::Duration};
mod button;
mod caption;
mod icons;
mod interaction;
pub(crate) mod localized_input;
mod popover;
mod style;
pub(crate) use button::*;
pub(crate) use caption::caption;
pub(crate) use icons::*;
pub(crate) use interaction::*;
pub(crate) use popover::*;
pub(crate) use style::*;

mod textarea;
pub(crate) use textarea::TextArea;
mod select;
pub(crate) use select::{language_select, select};
mod menu;
pub(crate) use menu::*;

#[cfg(feature = "test-utils")]
pub(crate) mod gallery;

mod form;
pub(crate) use form::*;

mod tooltip;
pub(crate) use tooltip::{anchored_tooltip, close_button_tooltips};
