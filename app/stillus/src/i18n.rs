// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

//! Offline interface messages. User text never passes through catalog lookup.
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use fluent_bundle::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! locales {
    ($( $name:ident, $code:literal, $tag:literal, $native:literal; )+) => {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        pub(crate) enum Locale { #[default] English, $( $name, )+ }
        impl Locale {
            pub(crate) const ALL: &[Self] = &[Self::English, $(Self::$name,)+];
            pub(crate) fn code(self) -> &'static str {
                match self { Self::English => "en", $(Self::$name => $code,)+ }
            }
            fn tag(self) -> &'static str {
                match self { Self::English => "en", $(Self::$name => $tag,)+ }
            }
            pub(crate) fn native_name(self) -> &'static str {
                match self { Self::English => "English", $(Self::$name => $native,)+ }
            }
            fn resource(self) -> &'static str {
                match self {
                    Self::English => include_str!("../locales/en.ftl"),
                    $(Self::$name => include_str!(concat!("../locales/", $code, ".ftl")),)+
                }
            }
        }
    };
}

locales! {
    Spanish, "es", "es", "Español";
    Russian, "ru", "ru", "Русский";
    ChineseSimplified, "zh/hans", "zh-Hans", "简体中文";
    ChineseTraditional, "zh/hant", "zh-Hant", "繁體中文";
    PortugueseBrazil, "pt/br", "pt-BR", "Português (Brasil)";
    PortuguesePortugal, "pt/pt", "pt-PT", "Português (Portugal)";
    Hindi, "hi", "hi", "हिन्दी";
    Arabic, "ar", "ar", "العربية";
    French, "fr", "fr", "Français";
    Bengali, "bn", "bn", "বাংলা";
    Indonesian, "id", "id", "Bahasa Indonesia";
    Urdu, "ur", "ur", "اردو";
    German, "de", "de", "Deutsch";
    Japanese, "ja", "ja", "日本語";
    Turkish, "tr", "tr", "Türkçe";
    Korean, "ko", "ko", "한국어";
}

impl Locale {
    pub(crate) fn is_rtl(self) -> bool {
        matches!(self, Self::Arabic | Self::Urdu)
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.native_name())
    }
}

impl Serialize for Locale {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let code = value.as_str().unwrap_or_default();
        Ok(Self::ALL
            .iter()
            .copied()
            .find(|locale| locale.code() == code)
            .unwrap_or_default())
    }
}

macro_rules! message_keys {
    ($($key:ident,)+) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(crate) enum Key { $($key,)+ }
        impl Key {
            pub(super) const ALL: &[Self] = &[$(Self::$key,)+];
            pub(super) fn name(self) -> &'static str { match self { $(Self::$key => stringify!($key),)+ } }
        }
    };
}
#[path = "message_keys.rs"]
mod message_keys;
pub(crate) use message_keys::Key;

impl Key {
    pub(crate) fn message(self) -> Message {
        Message::new(self, Vec::new())
    }
}
impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message().fmt(formatter)
    }
}

// Native file filters require static strings. Cache at most one per language.
pub(crate) fn static_filter_name() -> &'static str {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| {
        Locale::ALL
            .iter()
            .map(|locale| {
                let name = Key::SupportedFiles.message().render_for(*locale);
                &*Box::leak(name.into_boxed_str())
            })
            .collect()
    });
    names[Locale::ALL
        .iter()
        .position(|locale| *locale == current())
        .unwrap_or(0)]
}

thread_local! {
    static LANGUAGE: RwSignal<Locale> = floem::reactive::Scope::new().create_rw_signal(Locale::English);
    static BUNDLES: RefCell<Vec<(Locale, FluentBundle<FluentResource>)>> = const { RefCell::new(Vec::new()) };
}
static CRASH_LANGUAGE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn shortcut_modifier() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+"
    }
}

pub(crate) fn current() -> Locale {
    LANGUAGE.with(SignalGet::get)
}
pub(crate) fn set_current(locale: Locale) {
    LANGUAGE.with(|signal| signal.set(locale));
    CRASH_LANGUAGE.store(
        Locale::ALL
            .iter()
            .position(|item| *item == locale)
            .unwrap_or(0),
        Ordering::Relaxed,
    );
}

pub(crate) fn crash_locale() -> Locale {
    Locale::ALL
        .get(CRASH_LANGUAGE.load(Ordering::Relaxed))
        .copied()
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Argument {
    Text(String),
    Number(f64),
    Message(Box<Message>),
}
impl From<String> for Argument {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<&str> for Argument {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}
impl From<&String> for Argument {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}
impl From<usize> for Argument {
    fn from(value: usize) -> Self {
        Self::Number(value as f64)
    }
}
impl From<u64> for Argument {
    fn from(value: u64) -> Self {
        Self::Number(value as f64)
    }
}
impl From<f64> for Argument {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}
impl From<Message> for Argument {
    fn from(value: Message) -> Self {
        Self::Message(Box::new(value))
    }
}

/// Only interface text and bounded diagnostic parameters belong in a message.
/// Never put note bodies or passwords in these arguments.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Message {
    key: Key,
    args: Vec<(&'static str, Argument)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiText {
    Message(Message),
    Technical(String),
    Failure { details: String },
    Joined(Vec<UiText>),
}
impl Default for UiText {
    fn default() -> Self {
        Self::Technical(String::new())
    }
}
impl From<Message> for UiText {
    fn from(value: Message) -> Self {
        Self::Message(value)
    }
}
impl From<String> for UiText {
    fn from(value: String) -> Self {
        Self::Technical(value)
    }
}
impl From<&str> for UiText {
    fn from(value: &str) -> Self {
        Self::Technical(value.to_owned())
    }
}
impl fmt::Display for UiText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => message.fmt(formatter),
            Self::Technical(text) => formatter.write_str(text),
            Self::Failure { details } => msg!(OperationFailed, "error" => details).fmt(formatter),
            Self::Joined(messages) => {
                for (index, message) in messages.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(". ")?;
                    }
                    message.fmt(formatter)?;
                }
                Ok(())
            }
        }
    }
}
impl From<&UiText> for Argument {
    fn from(value: &UiText) -> Self {
        match value {
            UiText::Message(message) => Self::Message(Box::new(message.clone())),
            UiText::Technical(text) => Self::Text(text.clone()),
            UiText::Failure { details } => {
                Self::Message(Box::new(msg!(OperationFailed, "error" => details)))
            }
            UiText::Joined(_) => Self::Text(value.to_string()),
        }
    }
}

impl Message {
    pub(crate) fn new(key: Key, args: Vec<(&'static str, Argument)>) -> Self {
        Self { key, args }
    }
    pub(crate) fn render(&self) -> String {
        self.render_for(current())
    }
    pub(crate) fn render_for(&self, locale: Locale) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in &self.args {
            args.set(
                *name,
                match value {
                    Argument::Text(text) => FluentValue::from(text.clone()),
                    Argument::Number(number) => FluentValue::from(*number),
                    Argument::Message(message) => FluentValue::from(message.render_for(locale)),
                },
            );
        }
        format_message(locale, self.key, &args)
            .or_else(|| format_message(Locale::English, self.key, &args))
            .unwrap_or_else(|| self.key.name().to_owned())
    }
}

impl fmt::Display for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

fn make_bundle(locale: Locale) -> FluentBundle<FluentResource> {
    let resource =
        FluentResource::try_new(locale.resource().to_owned()).expect("validated embedded catalog");
    let mut bundle = FluentBundle::new(vec![locale.tag().parse().expect("valid embedded locale")]);
    bundle
        .add_resource(resource)
        .expect("unique embedded message keys");
    debug_assert!(
        Key::ALL
            .iter()
            .all(|key| bundle.get_message(key.name()).is_some())
    );
    // Isolate interpolated filenames and numbers in bidirectional text.
    bundle.set_use_isolating(locale.is_rtl());
    bundle
}

fn format_message(locale: Locale, key: Key, args: &FluentArgs<'_>) -> Option<String> {
    BUNDLES.with(|cache| {
        let mut cache = cache.try_borrow_mut().ok()?;
        if !cache.iter().any(|(language, _)| *language == locale) {
            cache.push((locale, make_bundle(locale)));
        }
        let bundle = &cache.iter().find(|(language, _)| *language == locale)?.1;
        let pattern = bundle.get_message(key.name())?.value()?;
        let mut errors = Vec::new();
        let text = bundle
            .format_pattern(pattern, Some(args), &mut errors)
            .into_owned();
        errors.is_empty().then_some(text)
    })
}

macro_rules! msg {
    ($key:ident $(, $name:literal => $value:expr)* $(,)?) => {
        $crate::i18n::Message::new($crate::i18n::Key::$key, vec![$(($name, $crate::i18n::Argument::from($value))),*])
    };
}
pub(crate) use msg;
macro_rules! tr {
    ($($tokens:tt)*) => { $crate::i18n::msg!($($tokens)*).render() };
}
pub(crate) use tr;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_catalogs_have_exactly_the_english_keys_and_parameters() {
        fn entries(source: &str) -> std::collections::BTreeMap<&str, String> {
            let mut entries = std::collections::BTreeMap::new();
            let mut key = "";
            for line in source.lines() {
                if !line.starts_with(char::is_whitespace)
                    && let Some((name, value)) = line.split_once(" = ")
                {
                    key = name;
                    assert!(
                        entries.insert(key, value.to_owned()).is_none(),
                        "duplicate {name}"
                    );
                } else if line.starts_with(char::is_whitespace)
                    && let Some(value) = entries.get_mut(key)
                {
                    value.push_str(line);
                }
            }
            entries
        }
        fn variables(source: &str) -> std::collections::BTreeSet<String> {
            source
                .split('$')
                .skip(1)
                .map(|part| {
                    part.chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect()
                })
                .collect()
        }
        let english = entries(Locale::English.resource());
        for locale in Locale::ALL {
            let bundle = make_bundle(*locale);
            let translated = entries(locale.resource());
            for key in Key::ALL {
                assert!(
                    bundle
                        .get_message(key.name())
                        .and_then(|message| message.value())
                        .is_some()
                );
                let required = variables(&english[key.name()]);
                if matches!(*key, Key::ChooseStillusWorkspace | Key::CrashTitle) {
                    assert!(
                        translated[key.name()].contains("Stillus"),
                        "{} {} must preserve the product name",
                        locale.code(),
                        key.name()
                    );
                }
                assert_eq!(
                    required,
                    variables(&translated[key.name()]),
                    "{} {}",
                    locale.code(),
                    key.name()
                );
                for number in [0, 1, 2, 3, 11, 100] {
                    let mut args = FluentArgs::new();
                    for name in &required {
                        args.set(name, number);
                    }
                    assert!(
                        format_message(*locale, *key, &args).is_some(),
                        "{} {}",
                        locale.code(),
                        key.name()
                    );
                }
            }
            let keys = locale
                .resource()
                .lines()
                .filter_map(|line| line.split_once(" = ").map(|(key, _)| key))
                .collect::<Vec<_>>();
            assert_eq!(keys.len(), Key::ALL.len(), "{}", locale.code());
        }
    }
    #[test]
    fn messages_reformat_without_changing_arguments() {
        let message = msg!(Language);
        assert_eq!(message.render_for(Locale::English), "Language");
        assert_eq!(message.render_for(Locale::Russian), "Язык");
        assert_eq!(Locale::ALL.len(), 17);
        assert_eq!(
            Locale::ALL.iter().filter(|locale| locale.is_rtl()).count(),
            2
        );
    }

    #[test]
    fn plural_rules_and_bidirectional_arguments() {
        assert_eq!(
            msg!(NoteCount, "count" => 1usize).render_for(Locale::English),
            "1 note"
        );
        assert_eq!(
            msg!(NoteCount, "count" => 2usize).render_for(Locale::Russian),
            "2 заметки"
        );
        assert_eq!(
            msg!(NoteCount, "count" => 5usize).render_for(Locale::Russian),
            "5 заметок"
        );
        let message = msg!(OpenFailed, "value" => "/notes/example.md");
        let arabic = message.render_for(Locale::Arabic);
        assert!(arabic.contains("\u{2068}/notes/example.md\u{2069}"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_fonts_cover_every_catalog_script() {
        use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};
        for locale in Locale::ALL {
            let mut layout = TextLayout::new();
            let sample = locale.resource();
            layout.set_text(
                sample,
                AttrsList::new(
                    Attrs::new()
                        .family(&[FamilyOwned::SansSerif])
                        .font_size(14.0),
                ),
            );
            for run in layout.layout_runs() {
                for glyph in run.glyphs {
                    assert_ne!(
                        glyph.glyph_id,
                        0,
                        "missing glyph in {}: {:?}",
                        locale.code(),
                        &run.text[glyph.start..glyph.end]
                    );
                }
            }
        }
    }
}
