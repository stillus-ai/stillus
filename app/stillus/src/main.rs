// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]
#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

use application::ai as ai_service;
use application::journal as ai_journal;
mod ai_journal_view;
mod ai_settings;
mod application;
mod chat_view;
mod crash_dialog;
mod editor_geometry;
mod i18n;
mod ui;
use ui::*;
mod native_diagnostics;
mod restart;
mod rss_card;
mod rss_filters;
use application::global::GlobalApplication;
use application::preferences::Preferences as UiPreferences;
use application::rss as rss_service;
use application::settings;
#[cfg(test)]
mod test_support;
mod update;

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
#[cfg(any(test, feature = "test-utils"))]
use std::thread;
use std::time::Duration;

#[cfg(test)]
use application::runtime::is_current_search_generation;
use application::runtime::{
    SecurityActionOutcome, SidebarFilter, UnlockOutcome, WorkspaceSwitchBlocker,
    category_path_is_same_or_descendant, category_path_segments, note_matches_filter,
    sidebar_note_order_key,
};
use application::workspace::Workspace as WorkspaceSession;
use editor_geometry::{EditorTextGeometry, GeometryConfig, GeometryLine, MAX_GEOMETRY_ROWS};
use floem::action::exec_after;
use floem::event::{Event, EventListener, EventPropagation};
use floem::file::{FileDialogOptions, FileSpec};
use floem::file_action::open_file;
use floem::keyboard::{Key, KeyCode, Modifiers, NamedKey, PhysicalKey};
use floem::kurbo::{Point, Size};
use floem::pointer::PointerInputEvent;
use floem::prelude::*;
use floem::reactive::create_effect;
use floem::style::{CursorStyle, Style};
use floem::window::WindowConfig;
use floem::{AnyView, Application, Clipboard, View, ViewId, quit_app};
use i18n::{UiText, msg, tr};
use settings::{
    CategoryNoteSortSettings, GlobalSettings, NoteSortField, PersistedExternalFile,
    PersistedSidebarGroup, SidebarSettings, SortDirection, UiSettings, UpdateSettings,
    WindowSettings, relative_note_path, resolve_note_path,
};
use stillus_core::{
    CatalogOrderItem, CoreError, DocumentTarget, EditorCommand, ExternalFileSummary,
    FAVORITED_ORDER_KEY, IntegrityResolution, ItemId, NoteProtection, RecoveryStatus, RssEntry,
    RssSubscriptionSummary, SaveStatus, SecurePhase, SecureProgress, ToolbarAction,
    ViewportRequest, open_rss_original,
};
#[cfg(test)]
use stillus_core::{SecureCompletion, SecureJob};
use stillus_editor::{ByteRange, word_range_in_text};
use stillus_search::MatchKind;
#[cfg(test)]
use stillus_search::SearchResult;
use stillus_secure::MasterPassword;
use zeroize::{Zeroize, Zeroizing};

fn text(value: impl fmt::Display + 'static) -> floem::views::Label {
    label(move || value.to_string())
}

fn note_caption(note: &stillus_core::NoteSummary) -> UiText {
    if matches!(&note.availability, stillus_core::NoteAvailability::IoError(reason)
        if reason == "unsupported legacy protected format")
    {
        msg!(UnsupportedProtectedNote).into()
    } else {
        note.title.clone().into()
    }
}

const SYSTEM_OPEN_POLL_MS: u64 = 100;
const CARET_BLINK_MS: u64 = 530;
const SEARCH_RECONCILE_MS: u64 = 1_000;
const NOTE_FIND_MATCH_LIMIT: usize = 10_000;
const EDITOR_FONT_SIZE_PX: f64 = crate::ui::FONT_BODY;
const EDITOR_LINE_HEIGHT_MULTIPLIER: f32 = 1.6;
const EDITOR_LINE_HEIGHT_PX: f64 = 22.4;
const EDITOR_CHARACTER_WIDTH_PX: f64 = 8.4;
const EDITOR_PADDING_X_PX: f64 = 28.0;
const EDITOR_PADDING_Y_PX: f64 = 24.0;
const EDITOR_LINE_NUMBER_PADDING_LEFT_PX: f64 = 8.0;
const EDITOR_LINE_NUMBER_MIN_WIDTH_PX: f64 = 28.0;
const EDITOR_LINE_NUMBER_GAP_PX: f64 = 12.0;
/// Floem normalizes one discrete wheel tick to 60 logical pixels. Requiring
/// that full distance for one document line keeps small trackpad deltas from
/// turning into the former fixed three-line jump.
const EDITOR_WHEEL_PIXELS_PER_LINE: f64 = 60.0;
const EDITOR_WHEEL_MAX_PIXELS_PER_EVENT: f64 = 240.0;
const EDITOR_CARET_HEIGHT_PX: f64 = 18.0;
const EDITOR_SELECTION_HEIGHT_PX: f64 = 20.0;
const EDITOR_DEFAULT_COLUMNS: usize = 74;
const EDITOR_DEFAULT_ROWS: usize = 27;
const EDITOR_MIN_COLUMNS: usize = 8;
const EDITOR_SCROLLBAR_WIDTH_PX: f64 = 4.0;
const EDITOR_SCROLLBAR_INSET_PX: f64 = 4.0;
const EDITOR_SCROLLBAR_MIN_HEIGHT_PX: f64 = 24.0;
const SIDEBAR_MIN_WIDTH_PX: f64 = 180.0;
const SIDEBAR_MAX_WIDTH_PX: f64 = 480.0;
const SCROLLBAR_HIDE_MS: u64 = 1000;
const TAG_POPOVER_WIDTH_PX: f64 = 280.0;
const TAG_POPOVER_GAP_PX: f64 = 6.0;
const TAG_POPOVER_PADDING_PX: f64 = 10.0;
const TAG_POPOVER_ROW_HEIGHT_PX: f64 = 32.0;
const TAG_POPOVER_ROW_GAP_PX: f64 = 2.0;
/// Space kept on both sides of every divider line inside the tag popover.
const TAG_POPOVER_SECTION_GAP_PX: f64 = 8.0;
/// Eight full rows plus half of the ninth: the cut row shows the list scrolls.
const TAG_POPOVER_LIST_MAX_HEIGHT_PX: f64 =
    8.0 * (TAG_POPOVER_ROW_HEIGHT_PX + TAG_POPOVER_ROW_GAP_PX) + TAG_POPOVER_ROW_HEIGHT_PX / 2.0;
const TAG_POPOVER_SCROLLBAR_PX: f64 = 4.0;
/// The list scrollbar sits centered inside the right card padding: the card
/// keeps only this inset on the right and the list extends into the rest of
/// the padding, so rows and the footer input share one right edge and the
/// bar never covers them.
const TAG_POPOVER_SCROLLBAR_INSET_PX: f64 =
    (TAG_POPOVER_PADDING_PX - TAG_POPOVER_SCROLLBAR_PX) / 2.0;
const TAG_POPOVER_GUTTER_PX: f64 = TAG_POPOVER_PADDING_PX - TAG_POPOVER_SCROLLBAR_INSET_PX;
const TAG_POPOVER_CONTENT_WIDTH_PX: f64 = TAG_POPOVER_WIDTH_PX - 2.0 - 2.0 * TAG_POPOVER_PADDING_PX;
const SORT_POPOVER_WIDTH_PX: f64 = 248.0;
const PROTECTION_POPOVER_WIDTH_PX: f64 = 220.0;
/// The creation popover shares the sort popover width: the RSS form inside it
/// has to fit a full feed URL on one line.
const CREATE_POPOVER_WIDTH_PX: f64 = 248.0;
/// The RSS form is a form, not a menu: it keeps a roomier card padding than
/// the choice rows, which carry their own horizontal padding.
const RSS_FORM_PADDING_PX: f64 = 16.0;
const RSS_FORM_GAP_PX: f64 = 8.0;
/// Gap between toolbar controls, shared by the document header and the feed
/// toolbar so both read as one row of controls.
const TOOLBAR_ACTION_GAP_PX: f64 = 6.0;
const RSS_FORM_BUTTON_HEIGHT_PX: f64 = 32.0;
/// Reserved for the hint line so a one-line error does not move the buttons.
const RSS_FORM_STATUS_HEIGHT_PX: f64 = 16.0;
/// Pressed-in shade of `Palette::accent` for the primary form button hover.
const RSS_FORM_ACCENT_HOVER: Color = Color::rgb8(42, 74, 103);
const MAX_PASSWORD_BYTES: usize = 1_024;
/// The bundled monospace family gives the editor a stable, measured advance.
const EDITOR_FONT_CANDIDATES: [&str; 1] = [ui::MONO_FONT_FAMILY];
const EDITOR_FALLBACK_FONT_FAMILY: &str = ui::MONO_FONT_FAMILY;
/// One frame is held this long: slow enough to read as a lock opening, fast
/// enough to look alive next to the caret blink.
const DECRYPT_FRAME_MS: u64 = 150;

/// Editor font resolved at startup: the first installed candidate family and
/// its measured advance width, which every caret, selection and hit-test
/// position is derived from.
struct EditorFont {
    family: String,
    character_width: f64,
}

fn probe_editor_font() -> EditorFont {
    ui::register_fonts();
    use floem::text::{Attrs, AttrsList, FONT_SYSTEM, FamilyOwned, TextLayout};

    let family = {
        // The lock is released before layout: shaping takes it again.
        let font_system = FONT_SYSTEM.lock();
        let database = font_system.db();
        EDITOR_FONT_CANDIDATES
            .iter()
            .find(|candidate| {
                database.faces().any(|face| {
                    face.families
                        .iter()
                        .any(|(name, _)| name.as_str() == **candidate)
                })
            })
            .map(|candidate| (*candidate).to_owned())
    };
    let families = [family.as_ref().map_or(FamilyOwned::Monospace, |name| {
        FamilyOwned::Name(name.clone())
    })];
    let sample_length = 64_usize;
    let mut layout = TextLayout::new();
    layout.set_text(
        &"0".repeat(sample_length),
        AttrsList::new(
            Attrs::new()
                .family(&families)
                .font_size(EDITOR_FONT_SIZE_PX as f32),
        ),
    );
    let measured = layout.size().width / sample_length as f64;
    EditorFont {
        family: family.unwrap_or_else(|| EDITOR_FALLBACK_FONT_FAMILY.to_owned()),
        character_width: if measured.is_finite() && measured > 0.0 {
            measured
        } else {
            EDITOR_CHARACTER_WIDTH_PX
        },
    }
}

use crash_dialog::install as install_panic_logging;

fn main() -> Result<(), LaunchError> {
    install_panic_logging();
    ui::register_fonts();
    let launch = LaunchOptions::parse()?;
    if launch.restart_after_update
        && !restart::await_handoff().map_err(|error| LaunchError::Restart(error.to_string()))?
    {
        return Ok(());
    }
    let home = stillus_platform::home_directory();
    let application::global::GlobalLoad {
        store: mut global_store,
        settings: global_settings,
        diagnostic: global_diagnostic,
    } = GlobalApplication::load(home.as_deref());
    i18n::set_current(global_store.locale());
    #[cfg(feature = "test-utils")]
    if launch.smoke_panic {
        thread::spawn(|| panic!("synthetic protected body must never reach diagnostics"))
            .join()
            .expect("panic hook exits");
    }
    if let Some(diagnostic) = global_diagnostic.as_deref() {
        eprintln!("Stillus: {diagnostic}");
    }
    let explicit_workspace = launch.workspace.is_some();
    let startup = resolve_startup_workspace(
        launch.workspace.as_deref(),
        &global_settings,
        home.as_deref(),
        global_diagnostic,
    );
    let (model, store, settings, startup_prompt, opened_path) = match startup {
        StartupWorkspace::Open(workspace) => {
            let application::preferences::Load {
                store,
                settings,
                diagnostic,
            } = UiPreferences::load(&workspace);
            if let Some(diagnostic) = diagnostic {
                eprintln!("Stillus: {diagnostic}");
            }
            let restored_note = settings
                .selected_note
                .as_deref()
                .and_then(|path| resolve_note_path(&workspace, path));
            let selected_external = settings.selected_external.as_deref().map(Path::new);
            let mut model = AppModel::load_restoring_state(
                &workspace,
                restored_note.as_deref(),
                &settings.external_files,
                selected_external,
                settings.selected_rss.as_deref(),
            );
            model.restore_chat_selection(settings.selected_chat.as_deref());
            (model, store, settings, None, Some(workspace))
        }
        StartupWorkspace::Choose(prompt) => (
            AppModel::unloaded(),
            UiPreferences::unbound(),
            UiSettings::default(),
            Some(prompt),
            None,
        ),
    };
    let model = Rc::new(RefCell::new(model));
    if explicit_workspace && model.borrow().workspace.is_some() {
        match opened_path
            .as_deref()
            .expect("explicit workspace has an opened path")
            .canonicalize()
        {
            Ok(workspace) => {
                if let Err(error) = global_store.remember_workspace_from_ui(&workspace) {
                    eprintln!("Stillus: {error}");
                }
            }
            Err(error) => {
                eprintln!("Stillus: opened workspace path could not be normalized: {error}")
            }
        }
    }
    model.borrow_mut().set_editor_font(probe_editor_font());
    let settings_store = Rc::new(RefCell::new(store));
    model.borrow_mut().application.preferences = Some(settings_store.clone());
    let global_settings_store = Rc::new(RefCell::new(global_store));
    model.borrow_mut().application.global = Some(global_settings_store.clone());
    let final_settings_store = settings_store.clone();
    let final_model = model.clone();
    let pending_restart = Rc::new(RefCell::new(None::<restart::PendingRestart>));
    let restart_request = pending_restart.clone();
    let app = Application::new();
    if let Some(delay) = launch.smoke_exit_after {
        exec_after(delay, |_| quit_app());
    }
    let smoke = SmokeOptions {
        autosave: launch.smoke_autosave,
        restore: launch.smoke_restore,
        operations: launch.smoke_operations,
    };
    let initial_window = settings.window;
    let close_requested = create_rw_signal(false);
    let close_model = model.clone();
    let build_view = move |_| {
        app_view(
            model,
            settings_store,
            global_settings_store,
            settings,
            startup_prompt,
            UiLaunch {
                close_requested,
                smoke,
                external_paths: launch.external_paths,
                restart_request,
            },
        )
    };
    let window_config = Some(
        WindowConfig::default()
            .on_close_requested(move || {
                let ready = close_model.borrow_mut().request_close_after_save();
                if !ready {
                    close_requested.set(true);
                }
                ready
            })
            .title("Stillus")
            .size((initial_window.width, initial_window.height))
            .min_size((settings::MIN_WINDOW_WIDTH, settings::MIN_WINDOW_HEIGHT))
            .apply_default_theme(false),
    );
    #[cfg(feature = "test-utils")]
    let app = if std::env::var_os("STILLUS_TEST_COMPONENTS").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        app.window(move |_| ui::gallery::view(), window_config)
    } else {
        app.window(build_view, window_config)
    };
    #[cfg(not(feature = "test-utils"))]
    let app = app.window(build_view, window_config);
    app.run();
    native_diagnostics::emit(native_diagnostics::Stage::EventLoopExited);
    if let Err(error) = final_settings_store.borrow_mut().flush() {
        native_diagnostics::emit(native_diagnostics::Stage::FinalSettingsFailed);
        if pending_restart.borrow().is_some() {
            return Err(LaunchError::Restart(error.to_string()));
        }
        eprintln!("Stillus: {error}");
    } else {
        native_diagnostics::emit(native_diagnostics::Stage::FinalSettingsFlushed);
    }
    if let Err(error) = final_model.borrow_mut().shutdown() {
        if pending_restart.borrow().is_some() {
            return Err(LaunchError::Restart(error));
        }
        eprintln!("Stillus: {error}");
    }
    if let Some(restart) = pending_restart.borrow_mut().take() {
        restart
            .complete()
            .map_err(|error| LaunchError::Restart(error.to_string()))?;
    }
    native_diagnostics::emit(native_diagnostics::Stage::ShutdownComplete);
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StartupWorkspacePrompt {
    candidate: Option<PathBuf>,
    diagnostic: Option<String>,
}

#[derive(Clone, Copy)]
struct StartupWorkspaceSignals {
    open: RwSignal<bool>,
    candidate: RwSignal<Option<PathBuf>>,
    diagnostic: RwSignal<Option<String>>,
    may_create_root: RwSignal<bool>,
    picker_active: RwSignal<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StartupCandidateState {
    Ready,
    NeedsInitialization(String),
    Invalid(String),
}

impl StartupCandidateState {
    fn primary_label(&self) -> String {
        match self {
            Self::NeedsInitialization(_) => tr!(CreateOpen),
            Self::Ready | Self::Invalid(_) => tr!(Open),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Ready => tr!(WorkspaceReady),
            Self::NeedsInitialization(detail) | Self::Invalid(detail) => detail.clone(),
        }
    }

    fn can_open(&self) -> bool {
        !matches!(self, Self::Invalid(_))
    }

    fn needs_initialization(&self) -> bool {
        matches!(self, Self::NeedsInitialization(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StartupWorkspace {
    Open(PathBuf),
    Choose(StartupWorkspacePrompt),
}

#[derive(Clone, Copy)]
struct SmokeOptions {
    autosave: bool,
    restore: bool,
    operations: bool,
}

fn resolve_startup_workspace(
    explicit_workspace: Option<&Path>,
    global_settings: &GlobalSettings,
    home: Option<&Path>,
    global_diagnostic: Option<String>,
) -> StartupWorkspace {
    if let Some(explicit) = explicit_workspace {
        return StartupWorkspace::Open(explicit.to_path_buf());
    }
    if let Some(remembered) = global_settings.workspace() {
        if workspace_is_available(&remembered) {
            return StartupWorkspace::Open(remembered);
        }
        return StartupWorkspace::Choose(StartupWorkspacePrompt {
            candidate: default_workspace_path(home),
            diagnostic: Some(
                tr!(SavedWorkspaceUnavailable , "value" => remembered.display() .to_string()),
            ),
        });
    }
    StartupWorkspace::Choose(StartupWorkspacePrompt {
        candidate: default_workspace_path(home),
        diagnostic: global_diagnostic,
    })
}

fn default_workspace_path(home: Option<&Path>) -> Option<PathBuf> {
    let downloads = home?.join("Downloads");
    fs::symlink_metadata(&downloads)
        .ok()
        .filter(|metadata| metadata.file_type().is_dir())
        .map(|_| downloads.join("Notes"))
}

fn workspace_is_available(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Ok(root) = fs::symlink_metadata(path) else {
        return false;
    };
    let Ok(notes) = fs::symlink_metadata(path.join("notes")) else {
        return false;
    };
    root.file_type().is_dir()
        && notes.file_type().is_dir()
        && fs::read_dir(path.join("notes")).is_ok()
}

fn startup_candidate_state(
    candidate: Option<&Path>,
    may_create_root: bool,
) -> StartupCandidateState {
    let Some(candidate) = candidate else {
        return StartupCandidateState::Invalid(tr!(DownloadsUnavailable));
    };
    if !candidate.is_absolute() {
        return StartupCandidateState::Invalid(tr!(AbsoluteWorkspace));
    }
    match fs::symlink_metadata(candidate) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return StartupCandidateState::Invalid(tr!(NotDirectory));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && may_create_root => {
            let Some(parent) = candidate.parent() else {
                return StartupCandidateState::Invalid(tr!(NoParent));
            };
            return match fs::symlink_metadata(parent) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    StartupCandidateState::NeedsInitialization(
                        tr!(CreateWorkspaceInfo , "value" => candidate.display() .to_string()),
                    )
                }
                Ok(_) => StartupCandidateState::Invalid(tr!(ParentNotDirectory)),
                Err(error) => StartupCandidateState::Invalid(
                    tr!(ParentUnavailable , "error" => error.to_string()),
                ),
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StartupCandidateState::Invalid(tr!(FolderGone));
        }
        Err(error) => {
            return StartupCandidateState::Invalid(
                tr!(CheckWorkspaceFailed , "error" => error.to_string()),
            );
        }
    }
    let notes = candidate.join("notes");
    match fs::symlink_metadata(&notes) {
        Ok(metadata) if metadata.file_type().is_dir() => StartupCandidateState::Ready,
        Ok(_) => StartupCandidateState::Invalid(
            tr!(PathExists , "value" => notes.display() .to_string()),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StartupCandidateState::NeedsInitialization(
                tr!(CreateNotesInfo , "value" => notes.display() .to_string()),
            )
        }
        Err(error) => {
            StartupCandidateState::Invalid(tr!(CheckNotesFailed , "error" => error.to_string()))
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct LaunchOptions {
    workspace: Option<PathBuf>,
    external_paths: Vec<PathBuf>,
    restart_after_update: bool,
    #[cfg(feature = "test-utils")]
    smoke_panic: bool,
    smoke_exit_after: Option<Duration>,
    smoke_autosave: bool,
    smoke_restore: bool,
    smoke_operations: bool,
}

impl LaunchOptions {
    fn parse() -> Result<Self, LaunchError> {
        let mut options = Self::parse_from(env::args_os().skip(1))?;
        let cwd =
            env::current_dir().map_err(|error| LaunchError::WorkingDirectory(error.to_string()))?;
        if let Some(path) = &mut options.workspace {
            if path.is_relative() {
                *path = cwd.join(&*path);
            }
        }
        for path in &mut options.external_paths {
            if path.is_relative() {
                *path = cwd.join(&*path);
            }
        }
        Ok(options)
    }

    fn parse_from<S: AsRef<std::ffi::OsStr>>(
        args: impl IntoIterator<Item = S>,
    ) -> Result<Self, LaunchError> {
        let mut workspace = None;
        let mut external_paths = Vec::new();
        let mut restart_after_update = false;
        let mut opening_files = false;
        let mut open_needs_value = false;
        let mut smoke_exit_after = None;
        #[cfg(feature = "test-utils")]
        let mut smoke_panic = false;
        let mut smoke_autosave = false;
        let mut smoke_restore = false;
        let mut smoke_operations = false;
        let mut positional_only = false;
        let mut args = args.into_iter().map(|arg| arg.as_ref().to_os_string());
        while let Some(argument) = args.next() {
            #[cfg(feature = "test-utils")]
            if !positional_only && argument == "--smoke-panic" {
                smoke_panic = true;
                continue;
            }
            if !positional_only
                && open_needs_value
                && argument.to_string_lossy().starts_with("--")
                && argument != "--"
            {
                return Err(LaunchError::MissingValue("--open"));
            }
            if !positional_only && argument == "--" {
                positional_only = true;
            } else if !positional_only && argument == restart::HANDOFF_FLAG {
                if restart_after_update {
                    return Err(LaunchError::UnexpectedArgument(
                        restart::HANDOFF_FLAG.to_owned(),
                    ));
                }
                restart_after_update = true;
            } else if !positional_only && argument == "--workspace" {
                if workspace.is_some() {
                    return Err(LaunchError::UnexpectedArgument("--workspace".to_owned()));
                }
                workspace = Some(PathBuf::from(
                    args.next()
                        .ok_or(LaunchError::MissingValue("--workspace"))?,
                ));
                opening_files = false;
            } else if !positional_only && argument == "--open" {
                opening_files = true;
                open_needs_value = true;
            } else if !positional_only && argument == "--smoke-exit-ms" {
                let value = args
                    .next()
                    .ok_or(LaunchError::MissingValue("--smoke-exit-ms"))?;
                let milliseconds = value
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        LaunchError::InvalidSmokeExit(value.to_string_lossy().into_owned())
                    })?;
                smoke_exit_after = Some(Duration::from_millis(milliseconds));
            } else if !positional_only && argument == "--smoke-autosave" {
                smoke_autosave = true;
            } else if !positional_only && argument == "--smoke-restore" {
                smoke_restore = true;
            } else if !positional_only && argument == "--smoke-operations" {
                smoke_operations = true;
            } else if !positional_only && argument.to_string_lossy().starts_with('-') {
                return Err(LaunchError::UnknownFlag(
                    argument.to_string_lossy().into_owned(),
                ));
            } else {
                let path = PathBuf::from(&argument);
                let file_argument = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| {
                        ["md", "markdown", "txt"]
                            .iter()
                            .any(|known| ext.eq_ignore_ascii_case(known))
                    });
                if opening_files || (!path.is_dir() && (file_argument || path.is_file())) {
                    external_paths.push(path);
                    open_needs_value = false;
                } else if workspace.is_none() {
                    workspace = Some(path);
                } else {
                    return Err(LaunchError::UnexpectedArgument(
                        argument.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        if open_needs_value {
            return Err(LaunchError::MissingValue("--open"));
        }
        Ok(Self {
            workspace,
            external_paths,
            restart_after_update,
            #[cfg(feature = "test-utils")]
            smoke_panic,
            smoke_exit_after,
            smoke_autosave,
            smoke_restore,
            smoke_operations,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LaunchError {
    Restart(String),
    WorkingDirectory(String),
    MissingValue(&'static str),
    InvalidSmokeExit(String),
    UnknownFlag(String),
    UnexpectedArgument(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restart(error) => write!(formatter, "cannot restart Stillus: {error}"),
            Self::WorkingDirectory(error) => {
                write!(formatter, "cannot resolve launch directory: {error}")
            }
            Self::MissingValue(flag) => {
                write!(formatter, "{}", tr!(FlagValue, "flag" => flag.to_string()))
            }
            Self::InvalidSmokeExit(value) => {
                write!(
                    formatter,
                    "{}",
                    tr!(SmokeInteger, "value" => value.to_string())
                )
            }
            Self::UnknownFlag(flag) => write!(
                formatter,
                "{}",
                tr!(UnknownFlag, "flag" => flag.to_string())
            ),
            Self::UnexpectedArgument(argument) => {
                write!(
                    formatter,
                    "{}",
                    tr!(ExtraArgument, "argument" => argument.to_string())
                )
            }
        }
    }
}

impl std::error::Error for LaunchError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasswordDialogKind {
    SetupProtection,
    ExistingProtection,
    Unlock { note_index: usize },
    UnlockForRecovery { note_index: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasswordField {
    Primary,
    Confirmation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtectionActionState {
    None,
    Protect,
    Lock,
    Decrypting,
    UnlockKnown { note_index: usize },
    Unlock { note_index: usize },
}

impl ProtectionActionState {
    fn icon(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Protect | Self::Lock | Self::Decrypting => Some(ICON_LOCK),
            Self::UnlockKnown { .. } | Self::Unlock { .. } => Some(ICON_UNLOCK),
        }
    }
}

#[cfg(test)]
use application::security::{
    PendingPasswordChange, PendingSecurityAction, SearchSecurityOperation,
};
use application::security::{PendingPasswordChangeState, SecureUiOperation};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasswordSubmitOutcome {
    Accepted {
        schedule_persistence: bool,
        close_dialog: bool,
    },
    AuthenticationFailed,
    OperationFailed,
}

fn recovery_password_outcome(unlock: UnlockOutcome) -> PasswordSubmitOutcome {
    match unlock {
        UnlockOutcome::Pending => PasswordSubmitOutcome::Accepted {
            schedule_persistence: true,
            close_dialog: false,
        },
        UnlockOutcome::AuthenticationFailed => PasswordSubmitOutcome::AuthenticationFailed,
        UnlockOutcome::OperationFailed => PasswordSubmitOutcome::OperationFailed,
    }
}

struct PasswordEntry {
    primary: Zeroizing<String>,
    confirmation: Zeroizing<String>,
    active: PasswordField,
}

#[derive(Clone, Copy)]
struct PasswordFieldIds {
    primary: ViewId,
    confirmation: ViewId,
}

impl PasswordFieldIds {
    fn get(self, field: PasswordField) -> ViewId {
        match field {
            PasswordField::Primary => self.primary,
            PasswordField::Confirmation => self.confirmation,
        }
    }

    fn other(self, field: PasswordField) -> PasswordField {
        match field {
            PasswordField::Primary => PasswordField::Confirmation,
            PasswordField::Confirmation => PasswordField::Primary,
        }
    }
}

#[derive(Clone, Copy)]
struct PasswordFocusSignals {
    field: RwSignal<Option<PasswordField>>,
    caret_visible: RwSignal<bool>,
    caret_focused: RwSignal<bool>,
    caret_generation: RwSignal<u64>,
}

impl Default for PasswordEntry {
    fn default() -> Self {
        Self {
            primary: Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES)),
            confirmation: Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES)),
            active: PasswordField::Primary,
        }
    }
}

impl PasswordEntry {
    fn clear(&mut self) {
        self.primary.zeroize();
        self.confirmation.zeroize();
        self.active = PasswordField::Primary;
    }

    fn active_mut(&mut self) -> &mut String {
        match self.active {
            PasswordField::Primary => &mut self.primary,
            PasswordField::Confirmation => &mut self.confirmation,
        }
    }

    fn push(&mut self, value: &str) -> bool {
        let active = self.active_mut();
        if active.len().saturating_add(value.len()) > MAX_PASSWORD_BYTES
            || active.len().saturating_add(value.len()) > active.capacity()
        {
            return false;
        }
        active.push_str(value);
        true
    }

    fn pop(&mut self) {
        self.active_mut().pop();
    }

    fn take_primary(&mut self) -> String {
        std::mem::replace(
            &mut *self.primary,
            String::with_capacity(MAX_PASSWORD_BYTES),
        )
    }
}

#[derive(Clone)]
struct SecurityUi {
    dialog: RwSignal<Option<PasswordDialogKind>>,
    entry: Rc<RefCell<PasswordEntry>>,
    entry_revision: RwSignal<u64>,
    feedback: RwSignal<Option<PasswordFeedback>>,
    busy: RwSignal<bool>,
}

#[derive(Clone, Debug, PartialEq)]
enum PasswordFeedback {
    Status(UiText),
    Error(UiText),
}

impl PasswordFeedback {
    fn message(&self) -> String {
        match self {
            Self::Status(message) | Self::Error(message) => message.to_string(),
        }
    }

    fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

#[derive(Clone)]
struct PanelContext {
    security: SecurityUi,
    palette: Palette,
}

impl SecurityUi {
    fn new() -> Self {
        Self {
            dialog: create_rw_signal(None),
            entry: Rc::new(RefCell::new(PasswordEntry::default())),
            entry_revision: create_rw_signal(0),
            feedback: create_rw_signal(None),
            busy: create_rw_signal(false),
        }
    }

    fn open(&self, kind: PasswordDialogKind) {
        self.entry.borrow_mut().clear();
        self.feedback.set(None);
        self.busy.set(false);
        self.dialog.set(Some(kind));
        self.entry_revision.update(|value| *value += 1);
    }

    fn close(&self) {
        self.entry.borrow_mut().clear();
        self.feedback.set(None);
        self.busy.set(false);
        self.dialog.set(None);
        self.entry_revision.update(|value| *value += 1);
    }

    fn clear_feedback(&self) {
        self.feedback.set(None);
    }

    fn set_error(&self, message: impl Into<UiText>) {
        self.feedback
            .set(Some(PasswordFeedback::Error(message.into())));
    }

    fn set_status(&self, message: impl Into<UiText>) {
        self.feedback
            .set(Some(PasswordFeedback::Status(message.into())));
    }

    fn authentication_failed(&self) {
        self.busy.set(false);
        self.entry.borrow_mut().clear();
        self.set_error(msg!(AuthenticationFailed));
        self.entry_revision.update(|value| *value += 1);
    }
}

struct AppModel {
    application: application::Application,
    viewport_first_line: usize,
    viewport_first_visual_row: usize,
    editor_columns: usize,
    editor_rows: usize,
    editor_font_family: String,
    editor_character_width: f64,
    editor_surface_width: f64,
    editor_surface_height: f64,
    editor_content_width: f64,
    editor_padding_x: f64,
    editor_wheel_remainder: f64,
    security_ui: Option<SecurityUi>,
    autosave_generation: u64,
    rss_filters_open: Option<RwSignal<bool>>,
}
impl std::ops::Deref for AppModel {
    type Target = application::Application;
    fn deref(&self) -> &Self::Target {
        &self.application
    }
}
impl std::ops::DerefMut for AppModel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.application
    }
}
impl AppModel {
    fn from_application(application: application::Application) -> Self {
        Self {
            application,
            viewport_first_line: 0,
            viewport_first_visual_row: 0,
            editor_columns: EDITOR_DEFAULT_COLUMNS,
            editor_rows: EDITOR_DEFAULT_ROWS,
            editor_font_family: EDITOR_FALLBACK_FONT_FAMILY.to_owned(),
            editor_character_width: EDITOR_CHARACTER_WIDTH_PX,
            editor_surface_width: 2.0 * EDITOR_PADDING_X_PX
                + EDITOR_DEFAULT_COLUMNS as f64 * EDITOR_CHARACTER_WIDTH_PX,
            editor_surface_height: 2.0 * EDITOR_PADDING_Y_PX
                + EDITOR_DEFAULT_ROWS as f64 * EDITOR_LINE_HEIGHT_PX,
            editor_content_width: EDITOR_DEFAULT_COLUMNS as f64 * EDITOR_CHARACTER_WIDTH_PX,
            editor_padding_x: EDITOR_LINE_NUMBER_MIN_WIDTH_PX + EDITOR_LINE_NUMBER_GAP_PX,
            editor_wheel_remainder: 0.0,
            security_ui: None,
            rss_filters_open: None,
            autosave_generation: 0,
        }
    }
    fn unloaded() -> Self {
        Self::from_application(application::Application::unloaded())
    }
    #[cfg(test)]
    fn load(path: &Path) -> Self {
        Self::from_application(application::Application::load(path))
    }
    #[cfg(test)]
    fn load_restoring(path: &Path, restored_note: Option<&Path>) -> Self {
        Self::from_application(application::Application::load_restoring(
            path,
            restored_note,
        ))
    }
    fn load_restoring_state(
        path: &Path,
        restored_note: Option<&Path>,
        restored_external_files: &[PersistedExternalFile],
        restored_external: Option<&Path>,
        restored_rss: Option<&str>,
    ) -> Self {
        Self::from_application(application::Application::load_restoring_state(
            path,
            restored_note,
            restored_external_files,
            restored_external,
            restored_rss,
        ))
    }

    fn scroll_lines(&mut self, delta: isize) -> bool {
        let max_first_line = self.max_viewport_first_line();
        let previous = self.viewport_first_line;
        let previous_visual_row = self.viewport_first_visual_row;
        self.viewport_first_line = self
            .viewport_first_line
            .saturating_add_signed(delta)
            .min(max_first_line);
        if self.viewport_first_line != previous || delta < 0 {
            self.viewport_first_visual_row = 0;
        }
        self.viewport_first_line != previous
            || self.viewport_first_visual_row != previous_visual_row
    }
    fn scroll_editor_wheel(&mut self, delta_y: f64) -> bool {
        let lines = editor_wheel_line_delta(&mut self.editor_wheel_remainder, delta_y);
        lines != 0 && self.scroll_lines(lines)
    }
    fn reveal_editor_selection(&mut self, selection: ByteRange) {
        if editor_selection_is_fully_visible(self, selection) {
            return;
        }
        let Some(target_line) = document_line_for_offset(self, selection.start().get()) else {
            return;
        };
        let mut best = target_line.min(self.max_viewport_first_line());
        self.viewport_first_line = best;
        self.viewport_first_visual_row = 0;

        if !editor_selection_is_fully_visible(self, selection) {
            let Some(first_match_row) = editor_selection_first_visual_row(self, selection) else {
                return;
            };
            self.viewport_first_visual_row = first_match_row;
            if !editor_selection_is_fully_visible(self, selection) {
                return;
            }
        }

        if self.viewport_first_visual_row > 0 {
            return;
        }

        // Restore as much nearby context as fits without ever pushing the
        // highlighted match back below the clipped editor surface.
        for _ in 0..self.editor_rows.max(1) {
            let Some(candidate) = best.checked_sub(1) else {
                break;
            };
            self.viewport_first_line = candidate;
            if !editor_selection_is_fully_visible(self, selection) {
                self.viewport_first_line = best;
                break;
            }
            best = candidate;
        }
    }
    fn max_viewport_first_line(&self) -> usize {
        let Some(document) = self.workspace.as_ref().and_then(WorkspaceSession::document) else {
            return 0;
        };
        let total = document.line_count();
        let rows = self.editor_rows.max(1);
        if total <= 1 {
            return 0;
        }
        // Every line occupies at least one row, so the answer is never earlier
        // than the last `rows` lines of the document.
        let Ok(snapshot) = document.viewport(ViewportRequest {
            first_line: total.saturating_sub(rows),
            visible_lines: rows,
            overscan_lines: 0,
        }) else {
            return total.saturating_sub(1);
        };
        let Some(geometry) = build_editor_geometry(self, &snapshot, MAX_GEOMETRY_ROWS, false)
        else {
            return total.saturating_sub(rows);
        };
        let tail = snapshot
            .lines
            .iter()
            .enumerate()
            .map(|(slot, line)| {
                (
                    line.line_index,
                    geometry
                        .rows()
                        .iter()
                        .filter(|row| row.line_slot == slot)
                        .count()
                        .max(1),
                )
            })
            .collect::<Vec<_>>();
        let fits_within = |budget: usize| {
            let mut accumulated = 0_usize;
            let mut first_line = total.saturating_sub(1);
            for (line_index, line_rows) in tail.iter().rev() {
                if accumulated.saturating_add(*line_rows) > budget {
                    break;
                }
                accumulated = accumulated.saturating_add(*line_rows);
                first_line = *line_index;
            }
            first_line
        };
        fits_within(rows)
    }
    fn set_editor_font(&mut self, font: EditorFont) {
        self.editor_font_family = font.family;
        self.editor_character_width = font.character_width;
        let (padding_x, content_width, columns) = editor_horizontal_metrics(self);
        self.editor_padding_x = padding_x;
        self.editor_content_width = content_width;
        self.editor_columns = columns;
    }
    fn update_editor_metrics(&mut self, width: f64, height: f64) -> bool {
        self.editor_surface_width = width;
        let (padding_x, content_width, columns) = editor_horizontal_metrics(self);
        let rows = ((height - 2.0 * EDITOR_PADDING_Y_PX) / EDITOR_LINE_HEIGHT_PX).floor();
        let rows = if rows.is_finite() && rows > 0.0 {
            (rows as usize).max(1)
        } else {
            1
        };
        let changed = columns != self.editor_columns
            || rows != self.editor_rows
            || (height - self.editor_surface_height).abs() > 0.25
            || (content_width - self.editor_content_width).abs() > 0.25
            || (padding_x - self.editor_padding_x).abs() > 0.25;
        self.editor_surface_height = height;
        self.editor_columns = columns;
        self.editor_rows = rows;
        self.editor_content_width = content_width;
        self.editor_padding_x = padding_x;
        changed
    }
    fn apply(&mut self, command: EditorCommand) -> Option<String> {
        self.editor_wheel_remainder = 0.0;
        let previous_first_line = self.viewport_first_line;
        let clipboard = self.application.apply(command);
        let Some(workspace) = self.workspace.as_ref() else {
            return clipboard;
        };
        let cursor_line = workspace
            .document()
            .and_then(|document| document.cursor_line().ok())
            .unwrap_or(0);
        let rows = self.editor_rows.max(1);
        if cursor_line < self.viewport_first_line {
            self.viewport_first_line = cursor_line;
        } else if cursor_line >= self.viewport_first_line + rows {
            self.viewport_first_line = cursor_line.saturating_sub(rows - 1);
        }
        if self.viewport_first_line != previous_first_line
            || self.viewport_first_visual_row > 0 && caret_geometry(self).is_none()
        {
            self.viewport_first_visual_row = 0;
        }
        clipboard
    }
    fn sync_effects(&mut self) {
        for event in std::mem::take(&mut self.application.effects) {
            use application::ApplicationEvent::*;
            match event {
                ResetEditor => {
                    self.viewport_first_line = 0;
                    self.viewport_first_visual_row = 0;
                    self.editor_wheel_remainder = 0.0;
                }
                event => {
                    if let Some(security) = &self.security_ui {
                        match event {
                            ClosePassword => security.close(),
                            CloseProtectionPassword
                                if security.dialog.get_untracked()
                                    == Some(PasswordDialogKind::ExistingProtection) =>
                            {
                                security.close()
                            }
                            AuthenticationFailed => security.authentication_failed(),
                            ProtectionAuthenticationFailed
                                if security.dialog.get_untracked()
                                    == Some(PasswordDialogKind::ExistingProtection) =>
                            {
                                security.authentication_failed()
                            }
                            PasswordFinished => security.busy.set(false),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    fn request_note_creation(&mut self, active: SidebarFilter) -> bool {
        let result = self.application.request_note_creation(active, tr!(NewNote));
        self.sync_effects();
        result
    }
    fn open_rss(&mut self, item_id: &ItemId) -> bool {
        let result = self.application.open_rss(item_id);
        self.sync_effects();
        result
    }
    fn create_rss(&mut self, url: &str, active: &SidebarFilter) -> Option<ItemId> {
        let result = self.application.create_rss(url, active);
        self.sync_effects();
        result
    }
    fn rss_command(&mut self, command: rss_service::Command) -> bool {
        let result = self.application.rss_command(command);
        self.sync_effects();
        result
    }
    fn start_rss_refresh(&mut self, item_id: ItemId) -> bool {
        let result = self.application.start_rss_refresh(item_id);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn poll_rss(&mut self) -> bool {
        let result = self.application.poll_rss();
        self.sync_effects();
        result
    }
    fn select_rss_entry(&mut self, entry_id: &str) -> bool {
        let result = self.application.select_rss_entry(entry_id);
        self.sync_effects();
        result
    }
    fn move_rss_selection(&mut self, direction: i32) -> bool {
        let result = self.application.move_rss_selection(direction);
        self.sync_effects();
        result
    }
    fn rename_selected_rss(&mut self, title: &str) -> bool {
        let result = self.application.rename_selected_rss(title);
        self.sync_effects();
        result
    }
    fn toggle_selected_rss_pinned(&mut self) -> bool {
        let result = self.application.toggle_selected_rss_pinned();
        self.sync_effects();
        result
    }
    fn toggle_selected_rss_favorited(&mut self) -> bool {
        let result = self.application.toggle_selected_rss_favorited();
        self.sync_effects();
        result
    }
    fn set_selected_rss_deleted(&mut self, deleted: bool) -> bool {
        let result = self.application.set_selected_rss_deleted(deleted);
        self.sync_effects();
        result
    }
    fn set_selected_rss_categories(&mut self, categories: Vec<String>) -> bool {
        let result = self.application.set_selected_rss_categories(categories);
        self.sync_effects();
        result
    }
    fn retry_password_change_recovery(&mut self) -> bool {
        let result = self.application.retry_password_change_recovery();
        self.sync_effects();
        result
    }
    fn start_integrity_resolution(&mut self, resolution: IntegrityResolution) -> bool {
        let result = self.application.start_integrity_resolution(resolution);
        self.sync_effects();
        result
    }
    fn request_master_password_change(
        &mut self,
        current: MasterPassword,
        new: MasterPassword,
    ) -> bool {
        let result = self
            .application
            .request_master_password_change(current, new);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn finish_secure_completion(&mut self, completion: SecureCompletion) -> bool {
        let result = self.application.finish_secure_completion(completion);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn finish_secure_progress(&mut self, progress: SecureProgress) -> bool {
        let result = self.application.finish_secure_progress(progress);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn shutdown_search_worker(&mut self) {
        self.application.shutdown_search_worker();
        self.sync_effects();
    }
    fn open_note(&mut self, index: usize) {
        self.application.open_note(index);
        self.sync_effects();
    }
    fn open_external_path(&mut self, path: &Path) -> bool {
        let result = self.application.open_external_path(path);
        self.sync_effects();
        result
    }
    fn accept_external_paths(&mut self, paths: &[PathBuf]) -> bool {
        let result = self.application.accept_external_paths(paths);
        self.sync_effects();
        result
    }
    fn close_external_target(&mut self, target: DocumentTarget) -> bool {
        let result = self.application.close_external_target(target);
        self.sync_effects();
        result
    }
    fn open_first_matching_note_if_unselected(&mut self, filter: &SidebarFilter) -> bool {
        let result = self
            .application
            .open_first_matching_note_if_unselected(filter);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn retry_pending_note_creation(&mut self) -> bool {
        let result = self.application.retry_pending_note_creation();
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn clear_category_note_order(&mut self, category: &str) -> Option<bool> {
        let result = self.application.clear_category_note_order(category);
        self.sync_effects();
        result
    }
    fn set_sidebar_catalog_order(
        &mut self,
        scope: &SidebarFilter,
        ordered: &[CatalogOrderItem],
    ) -> Option<bool> {
        let result = self.application.set_sidebar_catalog_order(scope, ordered);
        self.sync_effects();
        result
    }
    fn clear_sidebar_note_order(&mut self, scope: &SidebarFilter) -> Option<bool> {
        let result = self.application.clear_sidebar_note_order(scope);
        self.sync_effects();
        result
    }
    fn add_tag_selected(&mut self, tag: &str) -> bool {
        let result = self.application.add_tag_selected(tag);
        self.sync_effects();
        result
    }
    fn remove_tag_selected(&mut self, tag: &str) -> bool {
        let result = self.application.remove_tag_selected(tag);
        self.sync_effects();
        result
    }
    fn toggle_pinned_selected(&mut self) -> bool {
        let result = self.application.toggle_pinned_selected();
        self.sync_effects();
        result
    }
    fn toggle_favorited_selected(&mut self) -> bool {
        let result = self.application.toggle_favorited_selected();
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn start_optional_metadata_job(
        &mut self,
        result: Result<Option<SecureJob>, CoreError>,
    ) -> bool {
        let result = self.application.start_optional_metadata_job(result);
        self.sync_effects();
        result
    }
    fn set_deleted_selected(&mut self, deleted: bool) -> bool {
        let result = self.application.set_deleted_selected(deleted);
        self.sync_effects();
        result
    }
    fn submit_search(&mut self, query: String) {
        self.application.submit_search(query);
        self.sync_effects();
    }
    #[cfg(test)]
    fn request_search_reconcile(&mut self) {
        self.application.request_search_reconcile();
        self.sync_effects();
    }
    #[cfg(test)]
    fn next_search_operation_id(&mut self) -> u64 {
        let result = self.application.next_search_operation_id();
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn finish_search_purge(&mut self, operation_id: u64, result: Result<(), String>) -> bool {
        let result = self.application.finish_search_purge(operation_id, result);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn finish_search_restore(&mut self, operation_id: u64, result: Result<(), String>) -> bool {
        let result = self.application.finish_search_restore(operation_id, result);
        self.sync_effects();
        result
    }
    fn protect_selected(&mut self, password: Option<MasterPassword>) -> SecurityActionOutcome {
        let result = self.application.protect_selected(password);
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn retry_pending_security_action(&mut self) -> bool {
        let result = self.application.retry_pending_security_action();
        self.sync_effects();
        result
    }
    #[cfg(test)]
    fn invalidate_search_projection(&mut self) {
        self.application.invalidate_search_projection();
        self.sync_effects();
    }
    #[cfg(test)]
    fn accept_search_results(&mut self, generation: u64, results: Vec<SearchResult>) -> bool {
        let result = self.application.accept_search_results(generation, results);
        self.sync_effects();
        result
    }
    fn unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
        restore_recovery: bool,
    ) -> UnlockOutcome {
        let result = self
            .application
            .unlock_note(note_index, password, restore_recovery);
        self.sync_effects();
        result
    }
    fn lock_selected(&mut self) -> SecurityActionOutcome {
        let result = self.application.lock_selected();
        self.sync_effects();
        result
    }
    fn disable_protection_selected(&mut self) -> SecurityActionOutcome {
        let result = self.application.disable_protection_selected();
        self.sync_effects();
        result
    }
    fn restore_recovery_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        let result = self.application.restore_recovery_note(note_index);
        self.sync_effects();
        result
    }
    fn restore_selected_recovery(&mut self) -> Result<Option<usize>, CoreError> {
        let result = self.application.restore_selected_recovery();
        self.sync_effects();
        result
    }
    fn discard_local_and_reload(&mut self) -> Result<(), CoreError> {
        let result = self.application.discard_local_and_reload();
        self.sync_effects();
        result
    }
    fn open_search_result(&mut self, generation: u64, query: &str, relative_path: &str) -> bool {
        let result = self
            .application
            .open_search_result(generation, query, relative_path);
        self.sync_effects();
        result
    }
}

impl WorkspaceSwitchBlocker {
    fn message(self) -> i18n::Message {
        match self {
            Self::Persistence => msg!(WaitSave),
            Self::Security => msg!(WaitSecure),
            Self::Unsaved => msg!(WaitAutosave),
            Self::SaveFailure => msg!(ResolveSaveFirst),
        }
    }
}

fn workspace_switch_blocker(model: &AppModel) -> Option<WorkspaceSwitchBlocker> {
    application::runtime::workspace_switch_blocker(&model.application)
}

#[cfg(test)]
fn prepare_workspace_switch(
    path: &Path,
) -> Result<application::runtime::PreparedWorkspaceSwitch, UiText> {
    application::runtime::prepare_workspace_switch(path)
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn editor_line_number_width(line_count: usize, character_width: f64) -> f64 {
    (EDITOR_LINE_NUMBER_PADDING_LEFT_PX
        + decimal_digits(line_count.max(1)) as f64 * character_width)
        .max(EDITOR_LINE_NUMBER_MIN_WIDTH_PX)
}

fn editor_horizontal_metrics(model: &AppModel) -> (f64, f64, usize) {
    let line_count = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .map_or(1, |document| document.line_count());
    let padding_x = editor_line_number_width(line_count, model.editor_character_width)
        + EDITOR_LINE_NUMBER_GAP_PX;
    let content_width = (model.editor_surface_width - padding_x - EDITOR_PADDING_X_PX)
        .max(EDITOR_MIN_COLUMNS as f64 * model.editor_character_width);
    let columns = (content_width / model.editor_character_width).floor();
    let columns = if columns.is_finite() && columns > 0.0 {
        (columns as usize).max(EDITOR_MIN_COLUMNS)
    } else {
        EDITOR_MIN_COLUMNS
    };
    (padding_x, content_width, columns)
}

fn editor_scrollbar_thumb(model: &AppModel) -> Option<(f64, f64)> {
    let max_first_line = model.max_viewport_first_line();
    if max_first_line == 0 {
        return None;
    }
    let track_height = (model.editor_surface_height - 2.0 * EDITOR_SCROLLBAR_INSET_PX).max(0.0);
    if track_height == 0.0 {
        return None;
    }
    let visible_lines = model.editor_rows.max(1) as f64;
    let document_extent = max_first_line as f64 + visible_lines;
    let thumb_height = (track_height * visible_lines / document_extent).clamp(
        EDITOR_SCROLLBAR_MIN_HEIGHT_PX.min(track_height),
        track_height,
    );
    let progress = model.viewport_first_line.min(max_first_line) as f64 / max_first_line as f64;
    let top = EDITOR_SCROLLBAR_INSET_PX + (track_height - thumb_height) * progress;
    Some((top, thumb_height))
}

fn editor_wheel_line_delta(remainder: &mut f64, delta_y: f64) -> isize {
    if !remainder.is_finite() {
        *remainder = 0.0;
    }
    if !delta_y.is_finite() || delta_y == 0.0 {
        return 0;
    }
    let delta_y = delta_y.clamp(
        -EDITOR_WHEEL_MAX_PIXELS_PER_EVENT,
        EDITOR_WHEEL_MAX_PIXELS_PER_EVENT,
    );
    let accumulated = *remainder + delta_y;
    let lines = (accumulated / EDITOR_WHEEL_PIXELS_PER_LINE).trunc() as isize;
    *remainder = accumulated - lines as f64 * EDITOR_WHEEL_PIXELS_PER_LINE;
    lines
}

#[cfg(test)]
use application::search::search_worker;
#[cfg(test)]
use application::search::{SearchCommand, SearchEvent};

fn schedule_autosave(model: Rc<RefCell<AppModel>>, revision: RwSignal<u64>) {
    if revision.try_get_untracked().is_none() {
        return;
    }
    let (generation, delay) = {
        let mut model = model.borrow_mut();
        model.autosave_generation = model.autosave_generation.saturating_add(1);
        (
            model.autosave_generation,
            model.next_deadline().saturating_sub(model.now_ms()),
        )
    };
    exec_after(Duration::from_millis(delay), move |_| {
        autosave_tick(model, revision, generation)
    });
}

fn autosave_tick(model: Rc<RefCell<AppModel>>, revision: RwSignal<u64>, generation: u64) {
    if revision.try_get_untracked().is_none() {
        return;
    }
    let changed = {
        let mut model = model.borrow_mut();
        if model.autosave_generation != generation {
            return;
        }
        let changed = model.application.poll();
        model.sync_effects();
        changed
    };
    if changed {
        revision.update(|value| *value += 1);
    }
    if model.borrow().close_after_save_ready() {
        quit_app();
        return;
    }
    schedule_autosave(model, revision);
}

fn schedule_external_poll(model: Rc<RefCell<AppModel>>, revision: RwSignal<u64>) {
    schedule_autosave(model, revision);
}

fn schedule_rss_poll(model: Rc<RefCell<AppModel>>, revision: RwSignal<u64>) {
    schedule_autosave(model, revision);
}

// Keep startup requests outside AppModel: choosing a workspace replaces the model.
fn schedule_open_requests(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    pending: Rc<RefCell<Vec<PathBuf>>>,
) {
    exec_after(Duration::from_millis(SYSTEM_OPEN_POLL_MS), move |_| {
        if revision.try_get_untracked().is_none() {
            return;
        }
        #[cfg(target_os = "macos")]
        pending
            .borrow_mut()
            .extend(floem_winit::platform::macos::take_opened_files());
        let ready = {
            let model = model.borrow();
            model.workspace.is_some()
                && !model.secure_worker_active
                && model.pending_password_change.is_none()
                && model.pending_external_target.is_none()
        };
        if ready && !pending.borrow().is_empty() {
            let paths = std::mem::take(&mut *pending.borrow_mut());
            model.borrow_mut().accept_external_paths(&paths);
            // Errors also change the view, even if no file could be opened.
            revision.update(|value| *value = value.saturating_add(1));
            schedule_autosave(model.clone(), revision);
        }
        if cfg!(target_os = "macos") || !pending.borrow().is_empty() {
            schedule_open_requests(model, revision, pending);
        }
    });
}

fn schedule_search_poll(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    _search_open: RwSignal<bool>,
    _search_query: RwSignal<String>,
    _search_selected: RwSignal<usize>,
) {
    schedule_autosave(model, revision);
}

fn ui_settings_snapshot(
    model: &AppModel,
    window_size: Size,
    sidebar_width: f64,
    sidebar_state: &SidebarState,
) -> UiSettings {
    let categories = model
        .workspace
        .as_ref()
        .map(|workspace| {
            workspace
                .categories()
                .iter()
                .map(|category| category.name.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut reconciled_sidebar = sidebar_state.clone();
    reconciled_sidebar.reconcile_categories(categories);
    let selected_note = model.workspace.as_ref().and_then(|workspace| {
        workspace
            .selected_note()
            .and_then(|index| workspace.notes().get(index))
            .and_then(|note| relative_note_path(workspace.root(), &note.path))
    });
    let external_files = model
        .workspace
        .as_ref()
        .map(|workspace| {
            workspace
                .external_files()
                .iter()
                .filter_map(|file| {
                    Some(PersistedExternalFile {
                        engine_id: file.engine_id.as_str().to_owned(),
                        absolute_path: file.path.to_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let selected_external = model.workspace.as_ref().and_then(|workspace| {
        let DocumentTarget::ExternalFile { engine_id, item_id } = workspace.selected_target()?
        else {
            return None;
        };
        workspace
            .external_files()
            .iter()
            .find(|file| file.engine_id == engine_id && file.item_id == item_id)
            .and_then(|file| file.path.to_str().map(str::to_owned))
    });
    let selected_rss = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::selected_rss)
        .map(|item_id| item_id.as_str().to_owned());
    UiSettings {
        version: settings::SETTINGS_VERSION,
        window: WindowSettings {
            width: window_size.width,
            height: window_size.height,
        },
        sidebar: reconciled_sidebar.to_settings(sidebar_width),
        selected_note,
        external_files,
        selected_external,
        selected_rss,
        selected_chat: model
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_engine_item)
            .filter(|(e, _)| e == &stillus_chat::engine_id())
            .map(|(_, id)| id.to_string()),
    }
}

fn schedule_settings_save(
    store: Rc<RefCell<UiPreferences>>,
    _generation: RwSignal<u64>,
    snapshot: UiSettings,
) {
    store.borrow_mut().stage(snapshot);
}

#[derive(Clone)]
struct WorkspaceSwitchContext {
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    settings_generation: RwSignal<u64>,
    sidebar_width: RwSignal<f64>,
    sidebar_state: RwSignal<SidebarState>,
    search_open: RwSignal<bool>,
    search_query: RwSignal<String>,
    search_selected: RwSignal<usize>,
    note_find: NoteFindSignals,
    go_to_line: GoToLineSignals,
    tag_popover: TagPopoverSignals,
    security: SecurityUi,
}

type WorkspaceSwitchCompletion = Box<dyn FnOnce(Result<(PathBuf, Option<UiText>), UiText>)>;
fn switch_workspace(
    requested_path: &Path,
    initialize: bool,
    context: &WorkspaceSwitchContext,
    complete: WorkspaceSwitchCompletion,
) {
    let accepted = context
        .model
        .borrow_mut()
        .application
        .begin_workspace_switch(requested_path, initialize);
    match accepted {
        Ok(_) => {
            context.security.close();
            poll_workspace_switch(context.clone(), complete);
        }
        Err(error) => complete(Err(error.message)),
    }
}
fn poll_workspace_switch(context: WorkspaceSwitchContext, complete: WorkspaceSwitchCompletion) {
    exec_after(Duration::from_millis(25), move |_| {
        if context.revision.try_get_untracked().is_none() {
            return;
        }
        let result = context
            .model
            .borrow_mut()
            .application
            .take_workspace_switch_result();
        match result {
            Some(result) => {
                complete(result.and_then(|prepared| apply_workspace_projection(prepared, &context)))
            }
            None => poll_workspace_switch(context, complete),
        }
    });
}
fn apply_workspace_projection(
    prepared: application::runtime::WorkspaceChanged,
    context: &WorkspaceSwitchContext,
) -> Result<(PathBuf, Option<UiText>), UiText> {
    context.model.borrow_mut().sync_effects();
    if !prepared.changed {
        return Ok((prepared.canonical_path, prepared.diagnostic));
    }
    context.settings_generation.update(|value| {
        *value = value.saturating_add(1);
    });
    context.sidebar_width.set(prepared.settings.sidebar.width);
    let sidebar = {
        let model = context.model.borrow();
        let categories = model
            .workspace
            .as_ref()
            .map_or(&[][..], |workspace| workspace.categories());
        SidebarState::from_settings(
            &prepared.settings.sidebar,
            categories.iter().map(|category| category.name.as_str()),
        )
    };
    context.sidebar_state.set(sidebar);
    context.search_open.set(false);
    context.search_query.set(String::new());
    context.search_selected.set(0);
    close_note_find(context.note_find);
    close_go_to_line(context.go_to_line);
    close_tag_popover(context.tag_popover);
    context
        .revision
        .update(|value| *value = value.saturating_add(1));

    Ok((prepared.canonical_path, prepared.diagnostic))
}

struct UiLaunch {
    close_requested: RwSignal<bool>,
    smoke: SmokeOptions,
    external_paths: Vec<PathBuf>,
    restart_request: Rc<RefCell<Option<restart::PendingRestart>>>,
}

fn app_view(
    model: Rc<RefCell<AppModel>>,
    settings_store: Rc<RefCell<UiPreferences>>,
    global_settings_store: Rc<RefCell<GlobalApplication>>,
    initial_settings: UiSettings,
    startup_prompt: Option<StartupWorkspacePrompt>,
    launch: UiLaunch,
) -> impl IntoView {
    let UiLaunch {
        close_requested,
        smoke,
        external_paths,
        restart_request,
    } = launch;
    let revision = create_rw_signal(0_u64);
    let close_poll_model = model.clone();
    create_effect(move |_| {
        if close_requested.get() {
            close_requested.set(false);
            revision.update(|value| *value = value.saturating_add(1));
            schedule_autosave(close_poll_model.clone(), revision);
        }
    });
    let sidebar_width = create_rw_signal(initial_settings.sidebar.width);
    let sidebar_state = create_rw_signal({
        let model = model.borrow();
        let categories = model
            .workspace
            .as_ref()
            .map_or(&[][..], |workspace| workspace.categories());
        SidebarState::from_settings(
            &initial_settings.sidebar,
            categories.iter().map(|category| category.name.as_str()),
        )
    });
    let window_size = create_rw_signal(Size::new(
        initial_settings.window.width,
        initial_settings.window.height,
    ));
    let settings_generation = create_rw_signal(0_u64);
    let settings_page = SettingsPageSignals {
        open: create_rw_signal(false),
        section: create_rw_signal(SettingsSection::General),
        path: create_rw_signal(String::new()),
        feedback: create_rw_signal(None),
        picker_active: create_rw_signal(false),
        encryption_entry: create_rw_signal(EncryptionEntry::default()),
        encryption_revision: create_rw_signal(0),
        encryption_feedback: create_rw_signal(None),
    };
    let startup_workspace = StartupWorkspaceSignals {
        open: create_rw_signal(startup_prompt.is_some()),
        candidate: create_rw_signal(
            startup_prompt
                .as_ref()
                .and_then(|prompt| prompt.candidate.clone()),
        ),
        diagnostic: create_rw_signal(startup_prompt.and_then(|prompt| prompt.diagnostic)),
        may_create_root: create_rw_signal(true),
        picker_active: create_rw_signal(false),
    };
    let search_open = create_rw_signal(false);
    let search_query = create_rw_signal(String::new());
    let search_selected = create_rw_signal(0_usize);
    let editor_focus_request = create_rw_signal(0_u64);
    let creation_focus_model = model.clone();
    create_effect(move |_| {
        revision.get();
        let focus = {
            let mut model = creation_focus_model.borrow_mut();
            std::mem::take(&mut model.note_creation_focus_pending)
        };
        if focus {
            editor_focus_request.update(|value| *value = value.saturating_add(1));
        }
    });
    let note_find = NoteFindSignals {
        open: create_rw_signal(false),
        query: create_rw_signal(String::new()),
        selected: create_rw_signal(0_usize),
        matches: create_rw_signal(Vec::new()),
        focus_request: create_rw_signal(0_u64),
    };
    let go_to_line = GoToLineSignals {
        open: create_rw_signal(false),
        query: create_rw_signal(String::new()),
        error: create_rw_signal(None),
        focus_request: create_rw_signal(0_u64),
    };
    let tag_popover = TagPopoverSignals {
        open: create_rw_signal(false),
        target_path: create_rw_signal(None),
        query: create_rw_signal(String::new()),
        highlighted: create_rw_signal(None),
        hovered_tag: create_rw_signal(None),
        trigger_pointer_down: create_rw_signal(false),
    };
    let security = SecurityUi::new();
    model.borrow_mut().security_ui = Some(security.clone());
    let unlock_request_model = model.clone();
    let unlock_request_security = security.clone();
    create_effect(move |_| {
        revision.get();
        if let Some(note_index) = unlock_request_model.borrow_mut().unlock_request.take() {
            unlock_request_security.open(PasswordDialogKind::Unlock { note_index });
        }
    });
    let settings_effect_model = model.clone();
    let settings_effect_store = settings_store.clone();
    create_effect(move |_| {
        revision.get();
        if settings_effect_model.borrow().workspace.is_none()
            || settings_effect_model
                .borrow()
                .application
                .workspace_projection_pending()
        {
            return;
        }
        let projection = settings_effect_model
            .borrow_mut()
            .application
            .take_preferences_projection();
        if let Some(settings) = projection {
            sidebar_state.update(|state| {
                state.category_order = settings.category_order;
                state.note_sort = settings
                    .note_sort
                    .into_iter()
                    .map(|sort| {
                        (
                            sort.category,
                            NoteSort {
                                field: sort.field,
                                direction: sort.direction,
                            },
                        )
                    })
                    .collect();
            });
        }
        let snapshot = ui_settings_snapshot(
            &settings_effect_model.borrow(),
            window_size.get(),
            sidebar_width.get(),
            &sidebar_state.get(),
        );
        schedule_settings_save(settings_effect_store.clone(), settings_generation, snapshot);
    });
    schedule_external_poll(model.clone(), revision);
    schedule_rss_poll(model.clone(), revision);
    let restored_rss = {
        model
            .borrow()
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_rss)
            .cloned()
    };
    if let Some(item_id) = restored_rss
        && model.borrow_mut().start_rss_refresh(item_id)
    {
        schedule_rss_poll(model.clone(), revision);
    }
    schedule_open_requests(
        model.clone(),
        revision,
        Rc::new(RefCell::new(external_paths)),
    );
    schedule_search_poll(
        model.clone(),
        revision,
        search_open,
        search_query,
        search_selected,
    );
    let search_effect_model = model.clone();
    create_effect(move |_| {
        let query = search_query.get();
        search_selected.set(0);
        search_effect_model.borrow_mut().submit_search(query);
        revision.update(|value| *value += 1);
    });
    let search_tag_popover = tag_popover;
    let search_note_find = note_find;
    let search_go_to_line = go_to_line;
    create_effect(move |_| {
        if search_open.get() {
            close_tag_popover(search_tag_popover);
            close_note_find(search_note_find);
            close_go_to_line(search_go_to_line);
        }
    });
    let dialog_tag_popover = tag_popover;
    let dialog_go_to_line = go_to_line;
    let dialog_security = security.clone();
    create_effect(move |_| {
        if dialog_security.dialog.get().is_some() {
            close_tag_popover(dialog_tag_popover);
            close_go_to_line(dialog_go_to_line);
        }
    });
    if smoke.autosave {
        let smoke_model = model.clone();
        exec_after(Duration::from_millis(150), move |_| {
            smoke_model.borrow_mut().apply(EditorCommand::Insert(
                "[stillus autosave smoke]\n".to_owned(),
            ));
            revision.update(|value| *value += 1);
            schedule_autosave(smoke_model, revision);
        });
    }
    if smoke.restore {
        let restore_model = model.clone();
        exec_after(Duration::from_millis(150), move |_| {
            let result = {
                let mut model = restore_model.borrow_mut();
                let note_index = model
                    .workspace
                    .as_ref()
                    .ok_or_else(|| "workspace is not open".to_owned())
                    .and_then(|workspace| {
                        workspace
                            .selected_note()
                            .ok_or_else(|| "note is not selected".to_owned())
                    });
                note_index.and_then(|index| {
                    model
                        .restore_recovery_note(index)
                        .map_err(|error| error.to_string())
                })
            };
            if let Err(error) = result {
                restore_model.borrow_mut().error = Some(UiText::Failure {
                    details: error.to_string(),
                });
            }
            revision.update(|value| *value += 1);
            schedule_autosave(restore_model, revision);
        });
    }
    if smoke.operations {
        let operations_model = model.clone();
        exec_after(Duration::from_millis(150), move |_| {
            let succeeded = {
                let mut model = operations_model.borrow_mut();
                model
                    .application
                    .request_note_creation(SidebarFilter::All, "Smoke Note".into())
                    && model.rename_selected("Smoke Renamed")
                    && model.add_tag_selected("Smoke")
                    && model.toggle_pinned_selected()
                    && model.toggle_favorited_selected()
                    && model.set_deleted_selected(true)
            };
            if !succeeded && operations_model.borrow().error.is_none() {
                operations_model.borrow_mut().error =
                    Some(("operations smoke did not complete".to_owned()).into());
            }
            revision.update(|value| *value += 1);
        });
    }
    let palette = Palette::new();
    let panel_context = PanelContext {
        security: security.clone(),
        palette,
    };
    let workspace_switch = WorkspaceSwitchContext {
        model: model.clone(),
        revision,
        settings_generation,
        sidebar_width,
        sidebar_state,
        search_open,
        search_query,
        search_selected,
        note_find,
        go_to_line,
        tag_popover,
        security: security.clone(),
    };
    let apply_workspace: Rc<dyn Fn(PathBuf)> = {
        let context = workspace_switch.clone();
        Rc::new(move |path| {
            switch_workspace(
                &path,
                false,
                &context,
                Box::new(move |result| match result {
                    Ok((canonical_path, diagnostic)) => {
                        settings_page
                            .path
                            .set(canonical_path.to_string_lossy().into_owned());
                        settings_page.feedback.set(Some(SettingsFeedback {
                            message: diagnostic.unwrap_or_else(|| msg!(WorkspaceChanged).into()),
                            is_error: false,
                        }));
                    }
                    Err(message) => settings_page.feedback.set(Some(SettingsFeedback {
                        message,
                        is_error: true,
                    })),
                }),
            )
        })
    };
    let open_settings: Rc<dyn Fn()> = {
        let open_model = model.clone();
        Rc::new(move || {
            let path = open_model
                .borrow()
                .workspace
                .as_ref()
                .map(|workspace| workspace.root().to_string_lossy().into_owned())
                .unwrap_or_default();
            settings_page.path.set(path);
            settings_page.feedback.set(None);
            settings_page.encryption_feedback.set(None);
            settings_page.section.set(SettingsSection::General);
            settings_page.open.set(true);
            search_open.set(false);
            search_query.set(String::new());
            close_note_find(note_find);
            close_go_to_line(go_to_line);
            close_tag_popover(tag_popover);
        })
    };
    let close_settings_model = model.clone();
    let close_settings: Rc<dyn Fn()> = Rc::new(move || {
        let model = close_settings_model.borrow();
        if model.pending_password_change.is_some()
            || matches!(
                model.secure_ui_operation,
                Some(SecureUiOperation::ChangeMasterPassword)
            )
        {
            return;
        }
        drop(model);
        popover_close_all();
        settings_page.picker_active.set(false);
        settings_page.feedback.set(None);
        settings_page.encryption_feedback.set(None);
        settings_page
            .encryption_entry
            .update(EncryptionEntry::clear);
        settings_page
            .encryption_revision
            .update(|value| *value = value.saturating_add(1));
        settings_page.open.set(false);
    });
    let shell = h_stack((
        sidebar_panel(
            model.clone(),
            revision,
            SidebarLayoutSignals {
                sidebar_width,
                sidebar_state,
                window_size,
            },
            SearchPanelSignals {
                open: search_open,
                query: search_query,
                selected: search_selected,
                editor_focus_request,
            },
            open_settings,
            palette,
        ),
        main_content_panel(
            model.clone(),
            revision,
            EditorPanelSignals {
                tag_popover,
                sidebar_state,
                search_open,
                note_find,
                go_to_line,
                editor_focus_request,
            },
            panel_context,
            settings_page,
        ),
    ))
    .style(move |style| {
        rtl_row(style)
            .size_full()
            .min_size(settings::MIN_WINDOW_WIDTH, settings::MIN_WINDOW_HEIGHT)
            .background(palette.canvas)
            .color(palette.ink)
            .font_family(UI_FONT_FAMILY.to_owned())
            .font_size(crate::ui::FONT_BODY as f32)
            .line_height(1.35)
    });
    let restart_model = model.clone();
    let restart_settings = settings_store.clone();
    let restart_action = Rc::new(move |installation: &stillus_update::Installation| {
        if restart_request.borrow().is_some() {
            return Ok(true);
        }
        let model = restart_model.borrow();
        match workspace_switch_blocker(&model) {
            Some(WorkspaceSwitchBlocker::Unsaved) => {
                drop(model);
                schedule_autosave(restart_model.clone(), revision);
                return Ok(false);
            }
            Some(WorkspaceSwitchBlocker::Persistence) => return Ok(false),
            Some(blocker) => return Err(blocker.message().into()),
            None => {}
        }
        if !model.rss_refreshing.is_empty() || model.pending_note_creation.is_some() {
            return Ok(false);
        }
        let workspace = model.workspace.as_ref().map(WorkspaceSession::root);
        if workspace.is_some() {
            let snapshot = ui_settings_snapshot(
                &model,
                window_size.get_untracked(),
                sidebar_width.get_untracked(),
                &sidebar_state.get_untracked(),
            );
            restart_settings.borrow_mut().stage(snapshot);
            restart_settings.borrow_mut().flush().map_err(|error| {
                UiText::from(msg!(SaveSettingsFailed, "error" => error.to_string()))
            })?;
        }
        let pending = restart::PendingRestart::start(installation, workspace).map_err(|error| {
            UiText::from(msg!(UpdateRestartFailed, "error" => error.to_string()))
        })?;
        *restart_request.borrow_mut() = Some(pending);
        quit_app();
        Ok(true)
    });
    let updates = update::Updates::new(global_settings_store.clone(), restart_action);
    updates.start();
    let settings_overlay = settings_page_view(
        settings_page,
        revision,
        SettingsPageContext {
            global_settings_store: global_settings_store.clone(),
            model: model.clone(),
            apply_workspace,
            close_settings: close_settings.clone(),
            updates: updates.clone(),
            window_size,
        },
        palette,
    );
    let startup_overlay = startup_workspace_modal(startup_workspace, workspace_switch, palette);
    let root = stack((
        shell,
        settings_overlay,
        update::prompt_view(updates, palette),
        password_change_recovery_modal(model.clone(), revision, palette),
        integrity_modal(model.clone(), revision, palette),
        password_modal(model.clone(), security.clone(), revision, palette),
        startup_overlay,
    ))
    .style(|style| style.size_full());
    let overlay_owner_model = model.clone();
    create_effect(move |previous| {
        revision.get();
        let owner = {
            let model = overlay_owner_model.borrow();
            (
                settings_page.open.get(),
                settings_page.section.get(),
                model
                    .workspace
                    .as_ref()
                    .map(|workspace| workspace.root().to_owned()),
                model
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.selected_target()),
                model
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.selected_engine_item())
                    .cloned(),
            )
        };
        if previous.as_ref().is_some_and(|previous| previous != &owner) {
            popover_close_all();
        }
        owner
    });
    let root_find_model = model.clone();
    let root_go_to_line_model = model.clone();
    let resize_window_size = window_size;
    let close_settings_store = settings_store;
    root
        .on_event(EventListener::KeyDown, move |event| {
            if popover_handle_escape(event) {
                return EventPropagation::Stop;
            }
            if startup_workspace.open.get_untracked() {
                return EventPropagation::Stop;
            }
            if security.dialog.get_untracked().is_some() {
                if matches!(event, Event::KeyDown(key_event) if key_event.key.logical_key == Key::Named(NamedKey::Escape))
                {
                    security.close();
                }
                return EventPropagation::Stop;
            }
            if settings_page.open.get_untracked() {
                return EventPropagation::Stop;
            }
            let Event::KeyDown(key_event) = event else {
                return EventPropagation::Continue;
            };
            if let Some(open) = root_find_model.borrow().rss_filters_open
                && open.try_get_untracked() == Some(true) {
                if key_event.key.logical_key == Key::Named(NamedKey::Escape) { open.set(false); }
                return EventPropagation::Stop;
            }
            // Floem sends keys only to the focused view and then the window
            // root, not through the feed's ancestors. Handle unconsumed feed
            // navigation here so sidebar/buttons can retain keyboard focus.
            if !search_open.get_untracked()
                && key_event.modifiers.is_empty()
                && let Key::Character(character) = &key_event.key.logical_key
                && let Some(direction) = match character.as_str() {
                    "j" | "J" => Some(1),
                    "k" | "K" => Some(-1),
                    _ => None,
                }
                && root_find_model.borrow_mut().move_rss_selection(direction)
            {
                revision.update(|value| *value = value.saturating_add(1));
                return EventPropagation::Stop;
            }
            let transient_editor_action = if is_go_to_line_shortcut(key_event) {
                open_go_to_line(
                    &root_go_to_line_model,
                    go_to_line,
                    search_open,
                    note_find,
                    tag_popover,
                )
            } else if is_note_find_shortcut(key_event) {
                open_note_find(
                    &root_find_model,
                    note_find,
                    search_open,
                    tag_popover,
                    go_to_line,
                )
            } else {
                false
            };
            if transient_editor_action {
                EventPropagation::Stop
            } else if is_search_shortcut(key_event) {
                search_open.set(true);
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .on_event(EventListener::KeyUp, move |event| {
            if popover_handle_escape(event) {
                return EventPropagation::Stop;
            }
            if startup_workspace.open.get_untracked() {
                return EventPropagation::Stop;
            }
            let escape = matches!(event, Event::KeyUp(key_event) if key_event.key.logical_key == Key::Named(NamedKey::Escape));
            if settings_page.open.get_untracked() && escape {
                close_settings();
                EventPropagation::Stop
            } else if go_to_line.open.get_untracked() && escape {
                close_go_to_line(go_to_line);
                editor_focus_request.update(|value| *value = value.saturating_add(1));
                EventPropagation::Stop
            } else if note_find.open.get_untracked() && escape {
                close_note_find(note_find);
                editor_focus_request.update(|value| *value = value.saturating_add(1));
                EventPropagation::Stop
            } else if search_open.get_untracked() && escape
            {
                search_open.set(false);
                search_query.set(String::new());
                editor_focus_request.update(|value| *value = value.saturating_add(1));
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .on_event_cont(EventListener::WindowResized, move |event| {
            if let Event::WindowResized(size) = event {
                resize_window_size.set(*size);
            }
        })
        .on_event_cont(EventListener::WindowClosed, move |_| {
            native_diagnostics::emit(native_diagnostics::Stage::WindowClosed);
            if let Err(error) = close_settings_store.borrow_mut().flush() {
                native_diagnostics::emit(native_diagnostics::Stage::WindowSettingsFailed);
                eprintln!("Stillus: {error}");
            } else {
                native_diagnostics::emit(native_diagnostics::Stage::WindowSettingsFlushed);
            }
        })
}

/// Everything the settings overlay needs besides its own signals.
struct SettingsPageContext {
    global_settings_store: Rc<RefCell<GlobalApplication>>,
    model: Rc<RefCell<AppModel>>,
    apply_workspace: Rc<dyn Fn(PathBuf)>,
    close_settings: Rc<dyn Fn()>,
    updates: update::Updates,
    window_size: RwSignal<Size>,
}

fn settings_page_view(
    signals: SettingsPageSignals,
    revision: RwSignal<u64>,
    context: SettingsPageContext,
    palette: Palette,
) -> impl IntoView {
    let SettingsPageContext {
        global_settings_store,
        model,
        apply_workspace,
        close_settings,
        updates,
        window_size,
    } = context;
    let locale_projection = global_settings_store.clone();
    create_effect(move |_| {
        revision.get();
        let locale = locale_projection.borrow().locale();
        if i18n::current() != locale {
            i18n::set_current(locale);
        }
    });
    let ai_content = ai_settings::page(signals, revision, global_settings_store.clone(), palette);
    let updates_content = update::page(signals, updates, palette);
    let language_feedback = create_rw_signal(None::<i18n::Message>);
    let language_picker = language_select(
        move |locale| {
            // Release the coordinator before changing signals: locale effects read it again.
            let result = global_settings_store.borrow_mut().set_locale(locale);
            match result {
                Ok(()) => {
                    language_feedback.set(None);
                    i18n::set_current(locale);
                }
                Err(error) => language_feedback
                    .set(Some(msg!(LanguageSaveFailed, "error" => error.to_string()))),
            }
        },
        palette,
    );
    let language_card = settings_card(
        i18n::Key::Language,
        None,
        Some(i18n::Key::LanguageDescription),
        v_stack((
            language_picker,
            label(move || {
                language_feedback
                    .get()
                    .map(|message| message.render())
                    .unwrap_or_default()
            })
            .style(move |style| {
                style
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .color(palette.danger)
                    .apply_if(language_feedback.get().is_none(), |style| style.hide())
            }),
        ))
        .style(|style| style.width_full().gap(8.0)),
        palette,
    );
    let close_action = close_settings.clone();
    let general_navigation_model = model.clone();
    let encryption_navigation_model = model.clone();
    let ai_navigation_model = model.clone();
    let updates_navigation_model = model.clone();
    let navigation = v_stack((
        h_stack((
            icon_button(
                ButtonAction::Back.icon(),
                || tr!(BackToNotes),
                IconButtonTone::Sidebar,
                palette,
                move || close_action(),
            ),
            label(move || tr!(Settings)).style(move |style| {
                style
                    .font_size(crate::ui::FONT_SECTION as f32).font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                    .font_weight(floem::text::Weight::SEMIBOLD)
                    .color(palette.sidebar_ink)
                    .selectable(false)
            }),
        ))
        .style(|style| rtl_row(style).height(44.0).items_center().gap(10.0)),
        empty().style(|style| style.height(22.0)),
        label(move || tr!(Sections)).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.sidebar_muted)
                .selectable(false)
        }),
        empty().style(|style| style.height(8.0)),
        selectable_row(
            h_stack((
                svg(ButtonAction::Settings.icon()).style(|style| style.size(16.0, 16.0)),
                label(move || tr!(General)).style(|style| style.font_size(crate::ui::FONT_BODY as f32).selectable(false)),
            ))
            .style(|style| rtl_row(style).items_center().gap(10.0)),
            move || {
                if !password_change_busy(&general_navigation_model.borrow()) {
                    signals.section.set(SettingsSection::General);
                }
            },
        )
        .style(move |style| {
            rtl_row(style)
                .width_full()
                .height(38.0)
                .items_center()
                .padding_horiz(11.0)
                .background(if signals.section.get() == SettingsSection::General {
                    palette.sidebar_active
                } else {
                    Color::TRANSPARENT
                })
                .color(palette.sidebar_ink)
                .border_radius(6.0)
        }),
        selectable_row(
            h_stack((
                svg(ICON_LOCK).style(|style| style.size(16.0, 16.0)),
                label(move || tr!(Encryption))
                    .style(|style| style.font_size(crate::ui::FONT_BODY as f32).selectable(false)),
            ))
            .style(|style| rtl_row(style).items_center().gap(10.0)),
            move || {
                if !password_change_busy(&encryption_navigation_model.borrow()) {
                    signals.section.set(SettingsSection::Encryption);
                }
            },
        )
        .style(move |style| {
            rtl_row(style)
                .width_full()
                .height(38.0)
                .items_center()
                .padding_horiz(11.0)
                .background(if signals.section.get() == SettingsSection::Encryption {
                    palette.sidebar_active
                } else {
                    Color::TRANSPARENT
                })
                .color(palette.sidebar_ink)
                .border_radius(6.0)
        }),
        selectable_row(h_stack((
            svg(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><path d="m12 3 2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5Z"/></svg>"#).style(|style| style.size(16.0,16.0)),
            label(move || tr!(AiAssistant)).style(|style| style.selectable(false)),
        )).style(|style| rtl_row(style).items_center().gap(10.0)), move || {
            if !password_change_busy(&ai_navigation_model.borrow()) {
                signals.section.set(SettingsSection::Ai);
            }
        })
        .style(move |style| {
            rtl_row(style)
                .width_full()
                .height(38.0)
                .items_center()
                .padding_horiz(11.0)
                .font_size(crate::ui::FONT_BODY as f32)
                .border_radius(6.0)
                .color(palette.sidebar_ink)
                .background(if signals.section.get() == SettingsSection::Ai {
                    palette.sidebar_active
                } else {
                    Color::TRANSPARENT
                })
                .focus(|style| style.border(1.0).border_color(palette.accent))
        }),
        selectable_row(
            h_stack((
                svg(ICON_UPDATE).style(|style| style.size(16.0, 16.0)),
                label(move || tr!(Updates)).style(|style| style.font_size(crate::ui::FONT_BODY as f32).selectable(false)),
            ))
            .style(|style| rtl_row(style).items_center().gap(10.0)),
            move || {
                if !password_change_busy(&updates_navigation_model.borrow()) {
                    signals.section.set(SettingsSection::Updates);
                }
            },
        )
        .style(move |style| {
            rtl_row(style)
                .width_full()
                .height(38.0)
                .items_center()
                .padding_horiz(11.0)
                .background(if signals.section.get() == SettingsSection::Updates {
                    palette.sidebar_active
                } else {
                    Color::TRANSPARENT
                })
                .color(palette.sidebar_ink)
                .border_radius(6.0)
        }),
        empty().style(|style| style.flex_grow(1.0)),
    ))
    .style(move |style| {
        rtl_column(style)
            .width(232.0)
            .height_full()
            .flex_shrink(0.0)
            .padding(20.0)
            .background(palette.sidebar)
    });

    let path_input =
        localized_input::LocalizedInput::new(signals.path, i18n::Key::WorkspacePlaceholder)
            .style(move |style| settings_input_style(style, palette));

    let picker_apply = apply_workspace.clone();
    let picker_action = move || {
        if signals.picker_active.get_untracked() {
            return;
        }
        signals.picker_active.set(true);
        signals.feedback.set(None);
        let mut options = FileDialogOptions::new()
            .select_directories()
            .title(tr!(ChooseStillusWorkspace));
        let current = PathBuf::from(signals.path.get_untracked());
        if current.is_dir() {
            options = options.force_starting_directory(current);
        }
        let apply = picker_apply.clone();
        open_file(options, move |selection| {
            signals.picker_active.set(false);
            if let Some(path) = selection.and_then(|file| file.path.into_iter().next()) {
                signals.path.set(path.to_string_lossy().into_owned());
                apply(path);
            }
        });
    };
    let manual_apply = apply_workspace;
    let apply_action = move || {
        signals.feedback.set(None);
        let path = signals.path.get_untracked();
        if path.trim().is_empty() {
            signals.feedback.set(Some(SettingsFeedback {
                message: msg!(EnterWorkspace).into(),
                is_error: true,
            }));
            return;
        }
        manual_apply(PathBuf::from(path.trim()));
    };
    let controls = h_stack((
        dialog_button(
            ButtonAction::Custom(ICON_FOLDER),
            msg!(ChooseFolder),
            IconButtonTone::Secondary,
            palette,
            picker_action,
        ),
        dialog_button(
            ButtonAction::Custom(ButtonAction::Save.icon()),
            msg!(Apply),
            IconButtonTone::Primary,
            palette,
            apply_action,
        ),
    ))
    .style(|style| rtl_row(style).items_center().gap(8.0));

    let feedback = dyn_container(
        move || signals.feedback.get(),
        move |feedback| match feedback {
            Some(feedback) => text(feedback.message)
                .style(move |style| {
                    style
                        .font_size(crate::ui::FONT_CAPTION as f32)
                        .line_height(1.4)
                        .color(if feedback.is_error {
                            palette.danger
                        } else {
                            palette.accent
                        })
                        .selectable(false)
                })
                .into_any(),
            None => empty().style(|style| style.hide()).into_any(),
        },
    );

    let workspace_card = settings_card(
        i18n::Key::Workspace,
        Some(ICON_FOLDER),
        Some(i18n::Key::WorkspaceDescription),
        v_stack((
            settings_field_label(i18n::Key::Path, palette),
            path_input,
            controls,
            feedback,
        ))
        .style(|style| style.width_full().gap(8.0)),
        palette,
    );

    let general_content = scroll(
        v_stack((
            label(move || tr!(GeneralSettings)).style(move |style| {
                style
                    .font_size(crate::ui::FONT_SCREEN as f32)
                    .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                    .font_weight(floem::text::Weight::SEMIBOLD)
                    .color(palette.ink)
                    .selectable(false)
            }),
            empty().style(|style| style.height(7.0)),
            label(move || tr!(GeneralDescription)).style(move |style| {
                style
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.muted)
                    .selectable(false)
            }),
            empty().style(|style| style.height(28.0)),
            language_card,
            empty().style(|style| style.height(20.0)),
            workspace_card,
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
            .flex_basis(0.0)
            .flex_shrink(1.0)
            .height_full()
            .flex_grow(1.0)
            .background(palette.canvas)
    });

    let encryption_content = encryption_settings_view(signals, model, revision, palette);
    let content = stack((
        ai_content.style(move |style| {
            if signals.section.get() == SettingsSection::Ai {
                style
            } else {
                style.hide()
            }
        }),
        general_content.style(move |style| {
            if signals.section.get() == SettingsSection::General {
                style
            } else {
                style.hide()
            }
        }),
        encryption_content.style(move |style| {
            if signals.section.get() == SettingsSection::Encryption {
                style
            } else {
                style.hide()
            }
        }),
        updates_content.style(move |style| {
            if signals.section.get() == SettingsSection::Updates {
                style
            } else {
                style.hide()
            }
        }),
    ))
    .style(|style| {
        style
            .min_width(0.0)
            .flex_basis(0.0)
            .flex_shrink(1.0)
            .height_full()
            .flex_grow(1.0)
    });

    h_stack((navigation, content)).style(move |style| {
        let size = window_size.get();
        let style = rtl_row(style)
            .absolute()
            .size(size.width, size.height)
            .min_size(settings::MIN_WINDOW_WIDTH, settings::MIN_WINDOW_HEIGHT)
            .background(palette.canvas)
            .font_family(UI_FONT_FAMILY.to_owned());
        if signals.open.get() {
            style
        } else {
            style.hide()
        }
    })
}

fn startup_workspace_modal(
    signals: StartupWorkspaceSignals,
    context: WorkspaceSwitchContext,
    palette: Palette,
) -> impl IntoView {
    let picker_signals = signals;
    let picker_action = move || {
        if picker_signals.picker_active.get_untracked() {
            return;
        }
        picker_signals.picker_active.set(true);
        let mut options = FileDialogOptions::new()
            .select_directories()
            .title(tr!(ChooseStillusWorkspace));
        if let Some(candidate) = picker_signals.candidate.get_untracked() {
            let starting_directory = if candidate.is_dir() {
                Some(candidate)
            } else {
                candidate
                    .parent()
                    .filter(|parent| parent.is_dir())
                    .map(Path::to_path_buf)
            };
            if let Some(starting_directory) = starting_directory {
                options = options.force_starting_directory(starting_directory);
            }
        }
        open_file(options, move |selection| {
            picker_signals.picker_active.set(false);
            let Some(path) = selection.and_then(|file| file.path.into_iter().next()) else {
                return;
            };
            let path = path.canonicalize().unwrap_or(path);
            picker_signals.candidate.set(Some(path));
            picker_signals.may_create_root.set(false);
            picker_signals.diagnostic.set(None);
        });
    };

    let open_signals = signals;
    let open_context = context.clone();
    let open_action = move || {
        let candidate = open_signals.candidate.get_untracked();
        let state = startup_candidate_state(
            candidate.as_deref(),
            open_signals.may_create_root.get_untracked(),
        );
        if !state.can_open() {
            return;
        }
        let Some(candidate) = candidate else {
            return;
        };
        open_signals.may_create_root.set(false);
        let completion_context = open_context.clone();
        switch_workspace(
            &candidate,
            state.needs_initialization(),
            &open_context,
            Box::new(move |result| match result {
                Ok((_, diagnostic)) => {
                    open_signals.open.set(false);
                    open_signals.diagnostic.set(None);
                    if let Some(diagnostic) = diagnostic {
                        completion_context.model.borrow_mut().error = Some(diagnostic);
                        completion_context
                            .revision
                            .update(|value| *value = value.saturating_add(1));
                    }
                }
                Err(error) => open_signals.diagnostic.set(Some(error.to_string())),
            }),
        );
    };

    let disabled_signals = signals;
    let primary_label_signals = signals;
    let primary = content_button(
        ICON_FOLDER,
        label(move || {
            startup_candidate_state(
                primary_label_signals.candidate.get().as_deref(),
                primary_label_signals.may_create_root.get(),
            )
            .primary_label()
            .to_owned()
        })
        .style(|style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .selectable(false)
        }),
        open_action,
    )
    .disabled(move || {
        !startup_candidate_state(
            disabled_signals.candidate.get().as_deref(),
            disabled_signals.may_create_root.get(),
        )
        .can_open()
    })
    .style(move |style| {
        style
            .height(BUTTON_SIZE_PX)
            .padding_horiz(14.0)
            .items_center()
            .justify_center()
            .cursor(CursorStyle::Pointer)
            .background(Color::rgb8(48, 98, 143))
            .color(Color::WHITE)
            .border(1.0)
            .border_color(Color::rgb8(48, 98, 143))
            .border_radius(5.0)
            .hover(|style| {
                style
                    .background(Color::rgb8(35, 72, 105))
                    .border_color(Color::rgb8(35, 72, 105))
            })
            .disabled(move |style| {
                style
                    .cursor(CursorStyle::Default)
                    .background(Color::rgb8(166, 184, 200))
                    .border_color(Color::rgb8(166, 184, 200))
            })
    });

    let diagnostic_signals = signals;
    let diagnostic = dyn_container(
        move || diagnostic_signals.diagnostic.get(),
        move |diagnostic| match diagnostic {
            Some(diagnostic) => text(diagnostic)
                .style(move |style| {
                    style
                        .font_size(crate::ui::FONT_CAPTION as f32)
                        .line_height(1.4)
                        .color(palette.danger)
                        .selectable(false)
                })
                .into_any(),
            None => empty().style(|style| style.hide()).into_any(),
        },
    );
    let path_signals = signals;
    let path = label(move || {
        path_signals
            .candidate
            .get()
            .map_or_else(|| tr!(PathUnknown), |path| path.display().to_string())
    })
    .style(move |style| {
        style
            .width_full()
            .padding_vert(10.0)
            .padding_horiz(12.0)
            .background(palette.canvas)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(6.0)
            .font_size(crate::ui::FONT_BODY as f32)
            .selectable(true)
    });
    let detail_signals = signals;
    let detail = label(move || {
        detail_signals.diagnostic.get();
        startup_candidate_state(
            detail_signals.candidate.get().as_deref(),
            detail_signals.may_create_root.get(),
        )
        .detail()
    })
    .style(move |style| {
        let state = startup_candidate_state(
            detail_signals.candidate.get().as_deref(),
            detail_signals.may_create_root.get(),
        );
        style
            .font_size(crate::ui::FONT_CAPTION as f32)
            .line_height(1.4)
            .color(if matches!(state, StartupCandidateState::Invalid(_)) {
                palette.danger
            } else {
                palette.muted
            })
            .selectable(false)
    });
    let card = v_stack((
        h_stack((
            svg(ICON_FOLDER).style(move |style| style.size(24.0, 24.0).color(palette.accent)),
            label(move || tr!(ChooseWorkspace)).style(|style| {
                style
                    .font_size(crate::ui::FONT_SECTION as f32)
                    .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                    .font_weight(floem::text::Weight::SEMIBOLD)
                    .selectable(false)
            }),
        ))
        .style(|style| style.items_center().gap(10.0)),
        label(move || tr!(NotesDirectoryInfo)).style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .line_height(1.4)
                .color(palette.muted)
                .selectable(false)
        }),
        diagnostic,
        path,
        detail,
        h_stack((
            empty().style(|style| style.flex_grow(1.0)),
            dialog_button(
                ButtonAction::Custom(ICON_FOLDER),
                msg!(ChooseAnother),
                IconButtonTone::Secondary,
                palette,
                picker_action,
            ),
            primary,
        ))
        .style(|style| style.width_full().items_center().gap(8.0)),
    ))
    .style(move |style| dialog_card_style(style, palette, 520.0, 22.0).gap(14.0));
    container(card).style(move |style| {
        let style = style
            .absolute()
            .size_full()
            .items_center()
            .justify_center()
            .background(Color::rgba8(24, 29, 36, 112));
        if signals.open.get() {
            style
        } else {
            style.hide()
        }
    })
}

fn password_change_busy(model: &AppModel) -> bool {
    model.pending_password_change.is_some()
        || matches!(
            model.secure_ui_operation,
            Some(SecureUiOperation::ChangeMasterPassword)
        )
}

fn password_change_progress_text(message: String, progress: SecureProgress) -> String {
    match progress.percent {
        Some(percent) => format!("{percent}% · {message}"),
        None => message,
    }
}

fn password_change_success_text(notes: usize) -> String {
    tr!(PasswordChangeComplete , "notes" => notes)
}

fn submit_master_password_change(
    signals: SettingsPageSignals,
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
) {
    if password_change_busy(&model.borrow()) {
        return;
    }
    let (current, new_password, confirmation) = signals.encryption_entry.with_untracked(|entry| {
        (
            Zeroizing::new(entry.current.to_string()),
            Zeroizing::new(entry.new_password.to_string()),
            Zeroizing::new(entry.confirmation.to_string()),
        )
    });
    if current.is_empty() {
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message: msg!(EnterCurrentPassword).into(),
            is_error: true,
        }));
        return;
    }
    if new_password.is_empty() || confirmation.is_empty() {
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message: msg!(EnterRepeatNewPassword).into(),
            is_error: true,
        }));
        return;
    }
    if new_password.as_str() != confirmation.as_str() {
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message: msg!(NewPasswordsMismatch).into(),
            is_error: true,
        }));
        return;
    }
    if current.as_str() == new_password.as_str() {
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message: msg!(PasswordMustDiffer).into(),
            is_error: true,
        }));
        return;
    }
    let accepted = model.borrow_mut().request_master_password_change(
        MasterPassword::new(current.as_str().to_owned()),
        MasterPassword::new(new_password.as_str().to_owned()),
    );
    signals
        .encryption_entry
        .update(EncryptionEntry::clear_current);
    signals
        .encryption_revision
        .update(|value| *value = value.saturating_add(1));
    if accepted {
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message: msg!(PreparingPasswordChange).into(),
            is_error: false,
        }));
        schedule_autosave(model.clone(), revision);
    } else {
        let message = model
            .borrow()
            .password_change_error
            .clone()
            .unwrap_or_else(|| msg!(StartPasswordChangeFailed).into());
        signals.encryption_feedback.set(Some(SettingsFeedback {
            message,
            is_error: true,
        }));
    }
    revision.update(|value| *value = value.saturating_add(1));
}

fn encryption_settings_view(
    signals: SettingsPageSignals,
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let clear_model = model.clone();
    create_effect(move |_| {
        revision.get();
        if clear_model.borrow().password_change_result.is_some()
            && signals
                .encryption_entry
                .with(|entry| !entry.all_fields_empty())
        {
            signals.encryption_entry.update(EncryptionEntry::clear);
            signals
                .encryption_revision
                .update(|value| *value = value.saturating_add(1));
        }
    });
    let field_ids = Rc::new(Cell::new(None));
    let current = encryption_password_field(
        signals,
        EncryptionField::Current,
        i18n::Key::CurrentPassword,
        model.clone(),
        field_ids.clone(),
        revision,
        palette,
    );
    let current_id = current.id();
    let new_password = encryption_password_field(
        signals,
        EncryptionField::New,
        i18n::Key::NewPassword,
        model.clone(),
        field_ids.clone(),
        revision,
        palette,
    );
    let new_password_id = new_password.id();
    let confirmation = encryption_password_field(
        signals,
        EncryptionField::Confirmation,
        i18n::Key::RepeatNewPassword,
        model.clone(),
        field_ids.clone(),
        revision,
        palette,
    );
    let confirmation_id = confirmation.id();
    field_ids.set(Some(EncryptionFieldIds {
        current: current_id,
        new_password: new_password_id,
        confirmation: confirmation_id,
    }));

    let count_model = model.clone();
    let protected_count = label(move || {
        revision.get();
        let model = count_model.borrow();
        let (notes, recovery, secrets) = model.workspace.as_ref().map_or((0, 0, 0), |workspace| {
            (
                workspace.protected_note_count(),
                workspace.protected_recovery_count().unwrap_or(0),
                workspace.referenced_secret_count().unwrap_or(0),
            )
        });
        if secrets == 0 {
            tr!(ProtectedCounts , "notes" => notes, "recovery" => recovery)
        } else {
            tr!(ProtectedSecretCounts , "notes" => notes, "recovery" => recovery, "secrets" => secrets)
        }
    })
    .style(move |style| style.font_size(crate::ui::FONT_BODY as f32).color(palette.ink));

    let submit_model = model.clone();
    let disabled_model = model.clone();
    let submit = dialog_button(
        ButtonAction::Custom(ICON_LOCK),
        msg!(ChangeMasterPassword),
        IconButtonTone::Primary,
        palette,
        move || submit_master_password_change(signals, &submit_model, revision),
    )
    .disabled(move || {
        revision.get();
        let model = disabled_model.borrow();
        let configured = model
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::master_password_configured);
        password_change_busy(&model) || !configured
    })
    .style(move |style| disabled_control_style(style, palette));

    let status_model = model.clone();
    let status = label(move || {
        revision.get();
        let model = status_model.borrow();
        if let Some(error) = &model.password_change_error {
            return i18n::user_error_text(error);
        }
        if let Some(progress) = model.secure_progress {
            let message = match progress.phase {
                SecurePhase::Validating => {
                    tr!(CheckedProgress , "value" => progress.completed, "total" => progress.total)
                }
                SecurePhase::PreparingVerifier => {
                    tr!(VerifierPreparedProgress , "value" => progress.completed, "total" => progress.total)
                }
                SecurePhase::PreparingSecrets => tr!(SecretsPreparedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::PreparingNotes => {
                    tr!(PreparedProgress , "value" => progress.completed, "total" => progress.total)
                }
                SecurePhase::PreparingRecovery => tr!(RecoveryPreparedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::BackingUpNotes => tr!(BackupProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::BackingUpSecrets => tr!(SecretBackupProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::ReplacingRecovery => tr!(RecoveryReplacedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::ReplacingSecrets => tr!(SecretsReplacedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::ReplacingNotes => {
                    tr!(ReplacedProgress , "value" => progress.completed, "total" => progress.total)
                }
                SecurePhase::ReplacingVerifier => tr!(VerifierReplacedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::Verifying => tr!(VerifiedProgress , "value" => progress.completed, "total" => progress.total),
                SecurePhase::RollingBack => {
                    tr!(RestoredProgress , "value" => progress.completed, "total" => progress.total)
                }
            };
            return password_change_progress_text(message, progress);
        }
        if let Some((notes, _, _)) = model.password_change_result {
            return password_change_success_text(notes);
        }
        if let Some(request) = &model.pending_password_change {
            return match request.state {
                PendingPasswordChangeState::WaitingPersistence => {
                    tr!(WaitingAutosave)
                }
                PendingPasswordChangeState::WaitingSearch { .. } => {
                    tr!(PausingSearch)
                }
            };
        }
        if matches!(
            model.secure_ui_operation,
            Some(SecureUiOperation::ChangeMasterPassword)
        ) {
            return tr!(CheckingEncrypted);
        }
        signals
            .encryption_feedback
            .get()
            .map(|feedback| feedback.message.to_string())
            .unwrap_or_default()
    })
    .style(move |style| {
        revision.get();
        let is_error = model.borrow().password_change_error.is_some()
            || signals
                .encryption_feedback
                .get()
                .is_some_and(|feedback| feedback.is_error);
        let model = model.borrow();
        let visible = model.password_change_error.is_some() || model.secure_progress.is_some()
            || model.password_change_result.is_some() || model.pending_password_change.is_some()
            || matches!(model.secure_ui_operation, Some(SecureUiOperation::ChangeMasterPassword))
            || signals.encryption_feedback.get().is_some();
        style.apply_if(!visible, |style| style.hide()).font_size(crate::ui::FONT_CAPTION as f32).color(if is_error {
            palette.danger
        } else {
            palette.accent
        })
    });

    let card = settings_card(
        i18n::Key::ChangeMasterPassword,
        Some(ICON_LOCK),
        None,
        v_stack((
            protected_count,
            v_stack((current, new_password, confirmation))
                .style(|style| style.width_full().gap(8.0)),
            actions((submit,)),
            status,
        ))
        .style(|style| style.width_full().gap(16.0)),
        palette,
    );

    scroll(
        v_stack((
            label(move || tr!(Encryption)).style(move |style| {
                style
                    .font_size(crate::ui::FONT_SCREEN as f32)
                    .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                    .font_weight(floem::text::Weight::SEMIBOLD)
                    .color(palette.ink)
            }),
            empty().style(|style| style.height(7.0)),
            label(move || tr!(ChangePasswordDescription)).style(move |style| {
                style
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.muted)
            }),
            empty().style(|style| style.height(28.0)),
            card,
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

fn encryption_password_field(
    signals: SettingsPageSignals,
    field: EncryptionField,
    placeholder: i18n::Key,
    model: Rc<RefCell<AppModel>>,
    field_ids: Rc<Cell<Option<EncryptionFieldIds>>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl View {
    let label = label(move || {
        signals.encryption_revision.get();
        signals.encryption_entry.with(|entry| {
            let length = entry.field(field).chars().count();
            if length == 0 {
                placeholder.to_string()
            } else {
                "•".repeat(length)
            }
        })
    });
    let input_model = model.clone();
    let input_field_ids = field_ids;
    let disabled_model = model.clone();
    let focus_model = model.clone();
    MaskedPasswordView::new(
        label.style(|style| style.selectable(false)),
        move || {
            if password_change_busy(&model.borrow()) {
                return;
            }
            signals
                .encryption_entry
                .update(|entry| entry.active = field);
            signals.encryption_feedback.set(None);
            signals
                .encryption_revision
                .update(|value| *value = value.saturating_add(1));
        },
        move |event| {
            if password_change_busy(&input_model.borrow()) {
                return EventPropagation::Stop;
            }
            let append = |value: &str| {
                let mut accepted = false;
                signals.encryption_entry.update(|entry| {
                    entry.active = field;
                    let target = entry.field_mut(field);
                    if target.len().saturating_add(value.len()) <= MAX_PASSWORD_BYTES
                        && target.len().saturating_add(value.len()) <= target.capacity()
                    {
                        target.push_str(value);
                        accepted = true;
                    }
                });
                if accepted {
                    signals.encryption_feedback.set(None);
                } else {
                    signals.encryption_feedback.set(Some(SettingsFeedback {
                        message: msg!(PasswordTooLong , "maximum" => MAX_PASSWORD_BYTES).into(),
                        is_error: true,
                    }));
                }
                signals
                    .encryption_revision
                    .update(|value| *value = value.saturating_add(1));
            };
            if let Event::ImeCommit(value) = event {
                append(value);
                return EventPropagation::Stop;
            }
            let Event::KeyDown(key_event) = event else {
                return EventPropagation::Stop;
            };
            let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
                && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
            match &key_event.key.logical_key {
                Key::Named(NamedKey::Enter) => {
                    submit_master_password_change(signals, &input_model, revision);
                }
                Key::Named(NamedKey::Tab) => {
                    if let Some(ids) = input_field_ids.get() {
                        let target = ids.adjacent(field, key_event.modifiers.shift());
                        signals
                            .encryption_entry
                            .update(|entry| entry.active = target);
                        signals.encryption_feedback.set(None);
                        signals
                            .encryption_revision
                            .update(|value| *value = value.saturating_add(1));
                        ids.get(target).request_focus();
                    }
                }
                Key::Named(NamedKey::Backspace) => {
                    signals.encryption_entry.update(|entry| {
                        entry.active = field;
                        entry.field_mut(field).pop();
                    });
                    signals.encryption_feedback.set(None);
                    signals
                        .encryption_revision
                        .update(|value| *value = value.saturating_add(1));
                }
                Key::Character(value) if shortcut && value.to_lowercase() == "v" => {
                    match Clipboard::get_contents() {
                        Ok(value) => append(&value),
                        Err(_) => signals.encryption_feedback.set(Some(SettingsFeedback {
                            message: msg!(PastePasswordFailed).into(),
                            is_error: true,
                        })),
                    }
                }
                Key::Named(NamedKey::Space) if !shortcut => append(" "),
                Key::Character(value) if !shortcut => append(value),
                Key::Dead(_) => {}
                // Copy and cut are intentionally swallowed with every other
                // command shortcut so secrets never enter the clipboard.
                _ => {}
            }
            EventPropagation::Stop
        },
    )
    .style(move |style| {
        revision.get();
        signals.encryption_revision.get();
        let active = signals.encryption_entry.with(|entry| entry.active == field);
        let empty = signals
            .encryption_entry
            .with(|entry| entry.field(field).is_empty());
        settings_secret_style(style, palette, empty, active)
    })
    .style(move |style| disabled_control_style(style, palette))
    .disabled(move || {
        revision.get();
        password_change_busy(&disabled_model.borrow())
    })
    .keyboard_navigable()
    .on_event_stop(EventListener::FocusGained, move |_| {
        if password_change_busy(&focus_model.borrow()) {
            return;
        }
        signals
            .encryption_entry
            .update(|entry| entry.active = field);
        signals.encryption_feedback.set(None);
        signals
            .encryption_revision
            .update(|value| *value = value.saturating_add(1));
    })
}

fn password_change_recovery_modal(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let visibility_model = model.clone();
    let retry_model = model.clone();
    let error_model = model.clone();
    let card = v_stack((
        label(move || tr!(EncryptionRecoveryRequired)).style(|style| {
            style
                .font_size(crate::ui::FONT_SECTION as f32)
                .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                .font_weight(floem::text::Weight::SEMIBOLD)
        }),
        text(msg!(EncryptionRecoveryDescription)).style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .line_height(1.4)
                .color(palette.muted)
        }),
        label(move || {
            revision.get();
            error_model
                .borrow()
                .error
                .as_ref()
                .map(i18n::user_error_text)
                .unwrap_or_default()
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.danger)
        }),
        h_stack((
            empty().style(|style| style.flex_grow(1.0)),
            dialog_button(
                ButtonAction::Custom(ButtonAction::Refresh.icon()),
                msg!(RetryRecovery),
                IconButtonTone::Primary,
                palette,
                move || {
                    retry_model.borrow_mut().retry_password_change_recovery();
                    revision.update(|value| *value = value.saturating_add(1));
                },
            ),
        ))
        .style(|style| style.width_full()),
    ))
    .style(move |style| dialog_card_style(style, palette, 470.0, 20.0).gap(14.0));
    container(card).style(move |style| {
        revision.get();
        let style = modal_backdrop(style);
        if visibility_model
            .borrow()
            .blocked_password_change_workspace
            .is_some()
        {
            style
        } else {
            style.hide()
        }
    })
}

fn integrity_modal(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let state_model = model.clone();
    let visibility_model = model.clone();
    dyn_container(
        move || {
            revision.get();
            state_model
                .borrow()
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.integrity_failure().is_some())
        },
        move |visible| {
            if !visible {
                return empty().style(|style| style.hide()).into_any();
            }
            let retry_model = model.clone();
            let retry_disabled_model = model.clone();
            let restore_model = model.clone();
            let restore_disabled_model = model.clone();
            let error_model = model.clone();
            let error_visibility_model = model.clone();
            let retry = dialog_button(
                ButtonAction::Retry,
                msg!(Retry),
                IconButtonTone::Primary,
                palette,
                move || {
                    retry_model
                        .borrow_mut()
                        .start_integrity_resolution(IntegrityResolution::Retry);
                    revision.update(|value| *value += 1);
                },
            )
            .disabled(move || {
                revision.get();
                retry_disabled_model.borrow().secure_worker_active
            });
            let restore = dialog_button(
                ButtonAction::Custom(ICON_RECOVER),
                msg!(Restore),
                IconButtonTone::Danger,
                palette,
                move || {
                    restore_model
                        .borrow_mut()
                        .start_integrity_resolution(IntegrityResolution::Restore);
                    revision.update(|value| *value += 1);
                },
            )
            .disabled(move || {
                revision.get();
                restore_disabled_model.borrow().secure_worker_active
            });
            let error = label(move || {
                revision.get();
                error_model
                    .borrow()
                    .error
                    .as_ref()
                    .map(i18n::user_error_text)
                    .unwrap_or_default()
            })
            .style(move |style| {
                revision.get();
                let style = style
                    .min_height(16.0)
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .color(palette.danger);
                if error_visibility_model.borrow().error.is_some() {
                    style
                } else {
                    style.hide()
                }
            });
            let card = v_stack((
                v_stack((
                    label(move || tr!(VerifySaveFailed)).style(|style| {
                        style
                            .font_size(crate::ui::FONT_SECTION as f32)
                            .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                            .font_weight(floem::text::Weight::SEMIBOLD)
                    }),
                    text(msg!(PreviousEncryptedVersion)).style(move |style| {
                        style
                            .font_size(crate::ui::FONT_BODY as f32)
                            .line_height(1.4)
                            .color(palette.muted)
                    }),
                ))
                .style(|style| style.width_full().gap(6.0)),
                error,
                h_stack((empty().style(|style| style.flex_grow(1.0)), restore, retry))
                    .style(|style| style.width_full().items_center().gap(8.0)),
            ))
            .style(move |style| dialog_card_style(style, palette, 430.0, 20.0).gap(14.0));
            container(card).style(modal_backdrop).into_any()
        },
    )
    .style(move |style| {
        revision.get();
        let style = style.absolute().size_full();
        if visibility_model
            .borrow()
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.integrity_failure().is_some())
        {
            style
        } else {
            style.hide()
        }
    })
}

fn password_modal(
    model: Rc<RefCell<AppModel>>,
    security: SecurityUi,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let state_security = security.clone();
    let visibility_security = security.clone();
    dyn_container(
        move || state_security.dialog.get(),
        move |dialog| match dialog {
            Some(kind) => {
                password_dialog_card(kind, model.clone(), security.clone(), revision, palette)
                    .into_any()
            }
            None => empty().style(|style| style.hide()).into_any(),
        },
    )
    .style(move |style| {
        let style = style.absolute().size_full();
        if visibility_security.dialog.get().is_some() {
            style
        } else {
            style.hide()
        }
    })
}

fn password_dialog_card(
    kind: PasswordDialogKind,
    model: Rc<RefCell<AppModel>>,
    security: SecurityUi,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let is_setup = kind == PasswordDialogKind::SetupProtection;
    let title = match kind {
        PasswordDialogKind::SetupProtection => msg!(CreateMasterPassword),
        PasswordDialogKind::ExistingProtection => msg!(ConfirmMasterPassword),
        PasswordDialogKind::Unlock { .. } | PasswordDialogKind::UnlockForRecovery { .. } => {
            msg!(UnlockNote)
        }
    };
    let confirm_label = match kind {
        PasswordDialogKind::SetupProtection => msg!(Create),
        PasswordDialogKind::ExistingProtection => msg!(Confirm),
        PasswordDialogKind::Unlock { .. } | PasswordDialogKind::UnlockForRecovery { .. } => {
            msg!(Unlock)
        }
    };
    let detail = match kind {
        PasswordDialogKind::SetupProtection => {
            msg!(PasswordProtectsNotes)
        }
        PasswordDialogKind::ExistingProtection => msg!(ExistingPasswordPrompt),
        PasswordDialogKind::Unlock { .. } | PasswordDialogKind::UnlockForRecovery { .. } => {
            msg!(NotePasswordPrompt)
        }
    };

    let focus = PasswordFocusSignals {
        field: create_rw_signal(None),
        caret_visible: create_rw_signal(false),
        caret_focused: create_rw_signal(false),
        caret_generation: create_rw_signal(0),
    };
    let field_ids = Rc::new(Cell::new(None));

    let primary_security = security.clone();
    let primary_label_security = security.clone();
    let primary_style_security = security.clone();
    let primary_key_security = security.clone();
    let primary_key_model = model.clone();
    let primary_key_ids = field_ids.clone();
    let primary_leading_caret_security = security.clone();
    let primary_leading_caret = empty().style(move |style| {
        primary_leading_caret_security.entry_revision.get();
        let owns_position = primary_leading_caret_security
            .entry
            .borrow()
            .primary
            .is_empty();
        let visible = owns_position
            && focus.field.get() == Some(PasswordField::Primary)
            && focus.caret_visible.get();
        style
            .width(if owns_position { 1.0 } else { 0.0 })
            .height(18.0)
            .flex_shrink(0.0)
            .background(if visible {
                palette.accent
            } else {
                Color::TRANSPARENT
            })
    });
    let primary_trailing_caret_security = security.clone();
    let primary_trailing_caret = empty().style(move |style| {
        primary_trailing_caret_security.entry_revision.get();
        let owns_position = !primary_trailing_caret_security
            .entry
            .borrow()
            .primary
            .is_empty();
        let visible = owns_position
            && focus.field.get() == Some(PasswordField::Primary)
            && focus.caret_visible.get();
        style
            .width(if owns_position { 1.0 } else { 0.0 })
            .height(18.0)
            .flex_shrink(0.0)
            .background(if visible {
                palette.accent
            } else {
                Color::TRANSPARENT
            })
    });
    let primary_content = h_stack((
        primary_leading_caret,
        label(move || {
            primary_label_security.entry_revision.get();
            let len = primary_label_security
                .entry
                .borrow()
                .primary
                .chars()
                .count();
            if len == 0 {
                tr!(EnterPassword)
            } else {
                "•".repeat(len)
            }
        })
        .style(|style| style.min_width(0.0).flex_shrink(1.0).selectable(false)),
        primary_trailing_caret,
    ))
    .style(|style| style.width_full().min_width(0.0).items_center());
    let primary_field = MaskedPasswordView::new(
        primary_content,
        move || {
            if primary_security.busy.get_untracked() {
                return;
            }
            primary_security.entry.borrow_mut().active = PasswordField::Primary;
            primary_security.clear_feedback();
            primary_security.entry_revision.update(|value| *value += 1);
        },
        move |event| {
            let Some(ids) = primary_key_ids.get() else {
                return EventPropagation::Stop;
            };
            handle_password_key(
                event,
                (PasswordField::Primary, ids),
                kind,
                &primary_key_model,
                &primary_key_security,
                focus,
                revision,
            )
        },
    )
    .style(move |style| {
        primary_style_security.entry_revision.get();
        let active = focus.field.get() == Some(PasswordField::Primary);
        style
            .width_full()
            .height(38.0)
            .items_center()
            .cursor(CursorStyle::Text)
            .padding_horiz(11.0)
            .background(palette.paper)
            .color(
                if primary_style_security.entry.borrow().primary.is_empty() {
                    palette.muted
                } else {
                    palette.ink
                },
            )
            .border(1.0)
            .border_color(if active {
                palette.accent
            } else {
                palette.divider
            })
            .border_radius(6.0)
            .font_size(crate::ui::FONT_BODY as f32)
    })
    .keyboard_navigable();
    let primary_id = primary_field.id();

    let confirmation_security = security.clone();
    let confirmation_label_security = security.clone();
    let confirmation_style_security = security.clone();
    let confirmation_key_security = security.clone();
    let confirmation_key_model = model.clone();
    let confirmation_key_ids = field_ids.clone();
    let confirmation_leading_caret_security = security.clone();
    let confirmation_leading_caret = empty().style(move |style| {
        confirmation_leading_caret_security.entry_revision.get();
        let owns_position = confirmation_leading_caret_security
            .entry
            .borrow()
            .confirmation
            .is_empty();
        let visible = owns_position
            && focus.field.get() == Some(PasswordField::Confirmation)
            && focus.caret_visible.get();
        style
            .width(if owns_position { 1.0 } else { 0.0 })
            .height(18.0)
            .flex_shrink(0.0)
            .background(if visible {
                palette.accent
            } else {
                Color::TRANSPARENT
            })
    });
    let confirmation_trailing_caret_security = security.clone();
    let confirmation_trailing_caret = empty().style(move |style| {
        confirmation_trailing_caret_security.entry_revision.get();
        let owns_position = !confirmation_trailing_caret_security
            .entry
            .borrow()
            .confirmation
            .is_empty();
        let visible = owns_position
            && focus.field.get() == Some(PasswordField::Confirmation)
            && focus.caret_visible.get();
        style
            .width(if owns_position { 1.0 } else { 0.0 })
            .height(18.0)
            .flex_shrink(0.0)
            .background(if visible {
                palette.accent
            } else {
                Color::TRANSPARENT
            })
    });
    let confirmation_content = h_stack((
        confirmation_leading_caret,
        label(move || {
            confirmation_label_security.entry_revision.get();
            let len = confirmation_label_security
                .entry
                .borrow()
                .confirmation
                .chars()
                .count();
            if len == 0 {
                tr!(RepeatPassword)
            } else {
                "•".repeat(len)
            }
        })
        .style(|style| style.min_width(0.0).flex_shrink(1.0).selectable(false)),
        confirmation_trailing_caret,
    ))
    .style(|style| style.width_full().min_width(0.0).items_center());
    let confirmation_field = MaskedPasswordView::new(
        confirmation_content,
        move || {
            if confirmation_security.busy.get_untracked() {
                return;
            }
            confirmation_security.entry.borrow_mut().active = PasswordField::Confirmation;
            confirmation_security.clear_feedback();
            confirmation_security
                .entry_revision
                .update(|value| *value += 1);
        },
        move |event| {
            let Some(ids) = confirmation_key_ids.get() else {
                return EventPropagation::Stop;
            };
            handle_password_key(
                event,
                (PasswordField::Confirmation, ids),
                kind,
                &confirmation_key_model,
                &confirmation_key_security,
                focus,
                revision,
            )
        },
    )
    .style(move |style| {
        confirmation_style_security.entry_revision.get();
        let entry = confirmation_style_security.entry.borrow();
        let active = focus.field.get() == Some(PasswordField::Confirmation);
        let style = style
            .width_full()
            .height(38.0)
            .items_center()
            .cursor(CursorStyle::Text)
            .padding_horiz(11.0)
            .background(palette.paper)
            .color(if entry.confirmation.is_empty() {
                palette.muted
            } else {
                palette.ink
            })
            .border(1.0)
            .border_color(if active {
                palette.accent
            } else {
                palette.divider
            })
            .border_radius(6.0)
            .font_size(crate::ui::FONT_BODY as f32);
        if is_setup { style } else { style.hide() }
    })
    .keyboard_navigable();

    let confirmation_id = confirmation_field.id();
    let field_ids_value = PasswordFieldIds {
        primary: primary_id,
        confirmation: confirmation_id,
    };
    field_ids.set(Some(field_ids_value));

    let primary_focus_security = security.clone();
    let primary_field = primary_field
        .on_event_stop(EventListener::FocusGained, move |_| {
            primary_focus_security.entry.borrow_mut().active = PasswordField::Primary;
            focus.field.set(Some(PasswordField::Primary));
            focus.caret_focused.set(true);
            restart_caret_blink(
                focus.caret_visible,
                focus.caret_focused,
                focus.caret_generation,
            );
            primary_focus_security
                .entry_revision
                .update(|value| *value += 1);
        })
        .on_event_stop(EventListener::FocusLost, move |_| {
            // Floem can deliver the next field's FocusGained before this loss.
            if focus.field.get_untracked() == Some(PasswordField::Primary) {
                focus.field.set(None);
                stop_password_caret(focus);
            }
        });

    let confirmation_focus_security = security.clone();
    let confirmation_field = confirmation_field
        .on_event_stop(EventListener::FocusGained, move |_| {
            confirmation_focus_security.entry.borrow_mut().active = PasswordField::Confirmation;
            focus.field.set(Some(PasswordField::Confirmation));
            focus.caret_focused.set(true);
            restart_caret_blink(
                focus.caret_visible,
                focus.caret_focused,
                focus.caret_generation,
            );
            confirmation_focus_security
                .entry_revision
                .update(|value| *value += 1);
        })
        .on_event_stop(EventListener::FocusLost, move |_| {
            if focus.field.get_untracked() == Some(PasswordField::Confirmation) {
                focus.field.set(None);
                stop_password_caret(focus);
            }
        });

    let feedback_security = security.clone();
    let feedback_label = label(move || {
        feedback_security
            .feedback
            .get()
            .map(|feedback| feedback.message().to_owned())
            .unwrap_or_default()
    })
    .style(move |style| {
        let is_error = security
            .feedback
            .get()
            .is_some_and(|feedback| feedback.is_error());
        style
            .width_full()
            .height(16.0)
            .font_size(crate::ui::FONT_CAPTION as f32)
            .color(if is_error {
                palette.danger
            } else {
                palette.ink
            })
            .selectable(false)
    });

    let cancel_security = security.clone();
    let cancel_busy = security.busy;
    let submit_security = security.clone();
    let submit_busy = security.busy;
    let submit_model = model.clone();
    let warning = label(move || tr!(PasswordWarning)).style(move |style| {
        let style = style
            .width_full()
            .padding(10.0)
            .background(Color::rgb8(250, 246, 235))
            .color(Color::rgb8(114, 89, 42))
            .border_radius(6.0)
            .font_size(crate::ui::FONT_CAPTION as f32)
            .line_height(1.35);
        if is_setup { style } else { style.hide() }
    });
    let card = v_stack((
        v_stack((
            text(title).style(|style| {
                style
                    .font_size(crate::ui::FONT_SECTION as f32)
                    .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                    .font_weight(floem::text::Weight::SEMIBOLD)
            }),
            text(detail).style(move |style| {
                style
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.muted)
            }),
        ))
        .style(|style| style.width_full().gap(4.0)),
        warning,
        v_stack((primary_field, confirmation_field)).style(|style| style.width_full().gap(8.0)),
        feedback_label,
        h_stack((
            empty().style(|style| style.flex_grow(1.0)),
            password_dialog_button(
                ButtonAction::Cancel,
                msg!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || cancel_busy.get(),
                move || cancel_security.close(),
            ),
            password_dialog_button(
                ButtonAction::Custom(ICON_UNLOCK),
                confirm_label,
                IconButtonTone::Primary,
                palette,
                move || submit_busy.get(),
                move || {
                    if let Some(field) =
                        submit_password_dialog(kind, &submit_model, &submit_security, revision)
                    {
                        request_password_field_focus(field, field_ids_value, &submit_security);
                    }
                },
            ),
        ))
        .style(|style| style.width_full().items_center().gap(8.0)),
    ))
    .style(move |style| dialog_card_style(style, palette, 390.0, 20.0).gap(14.0));
    exec_after(Duration::from_millis(10), move |_| {
        primary_id.request_focus()
    });
    container(card).style(modal_backdrop)
}

fn stop_password_caret(focus: PasswordFocusSignals) {
    focus.caret_focused.set(false);
    focus.caret_visible.set(false);
    focus
        .caret_generation
        .update(|value| *value = value.saturating_add(1));
}

fn request_password_field_focus(
    field: PasswordField,
    ids: PasswordFieldIds,
    security: &SecurityUi,
) {
    security.entry.borrow_mut().active = field;
    security.entry_revision.update(|value| *value += 1);
    ids.get(field).request_focus();
}

fn handle_password_key(
    event: &Event,
    field: (PasswordField, PasswordFieldIds),
    kind: PasswordDialogKind,
    model: &Rc<RefCell<AppModel>>,
    security: &SecurityUi,
    focus: PasswordFocusSignals,
    revision: RwSignal<u64>,
) -> EventPropagation {
    let (field, field_ids) = field;
    if security.busy.get_untracked() {
        return EventPropagation::Stop;
    }
    if let Event::ImeCommit(value) = event {
        security.entry.borrow_mut().active = field;
        append_password_value(security, value);
        security.entry_revision.update(|value| *value += 1);
        restart_caret_blink(
            focus.caret_visible,
            focus.caret_focused,
            focus.caret_generation,
        );
        return EventPropagation::Stop;
    }
    let Event::KeyDown(key_event) = event else {
        return EventPropagation::Continue;
    };
    let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
        && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
    match &key_event.key.logical_key {
        Key::Named(NamedKey::Escape) => {
            security.close();
        }
        Key::Named(NamedKey::Enter) => {
            let advance_to_confirmation = kind == PasswordDialogKind::SetupProtection && {
                let entry = security.entry.borrow();
                field == PasswordField::Primary
                    && !entry.primary.is_empty()
                    && entry.confirmation.is_empty()
            };
            if advance_to_confirmation {
                request_password_field_focus(PasswordField::Confirmation, field_ids, security);
                security.clear_feedback();
            } else if let Some(target) = submit_password_dialog(kind, model, security, revision) {
                request_password_field_focus(target, field_ids, security);
            }
        }
        Key::Named(NamedKey::Backspace) => {
            security.entry.borrow_mut().active = field;
            security.entry.borrow_mut().pop();
            security.clear_feedback();
            security.entry_revision.update(|value| *value += 1);
        }
        Key::Named(NamedKey::Tab) if kind == PasswordDialogKind::SetupProtection => {
            request_password_field_focus(field_ids.other(field), field_ids, security);
        }
        Key::Character(value) if shortcut && value.to_lowercase() == "v" => {
            security.entry.borrow_mut().active = field;
            match Clipboard::get_contents() {
                Ok(value) => append_password_value(security, &value),
                Err(_) => security.set_error(msg!(PastePasswordFailed)),
            }
            security.entry_revision.update(|value| *value += 1);
        }
        Key::Named(NamedKey::Space) if !shortcut => {
            security.entry.borrow_mut().active = field;
            append_password_value(security, " ");
            security.entry_revision.update(|value| *value += 1);
        }
        Key::Character(value) if !shortcut => {
            security.entry.borrow_mut().active = field;
            append_password_value(security, value);
            security.entry_revision.update(|value| *value += 1);
        }
        // A dead key starts an OS composition. The committed character arrives
        // through ImeCommit; inserting this marker would duplicate accents.
        Key::Dead(_) => {}
        _ => return EventPropagation::Stop,
    }
    restart_caret_blink(
        focus.caret_visible,
        focus.caret_focused,
        focus.caret_generation,
    );
    EventPropagation::Stop
}

fn append_password_value(security: &SecurityUi, value: &str) {
    if security.entry.borrow_mut().push(value) {
        security.clear_feedback();
    } else {
        security.set_error(msg!(PasswordTooLong , "maximum" => MAX_PASSWORD_BYTES));
    }
}

fn submit_password_dialog(
    kind: PasswordDialogKind,
    model: &Rc<RefCell<AppModel>>,
    security: &SecurityUi,
    revision: RwSignal<u64>,
) -> Option<PasswordField> {
    if security.busy.get_untracked() {
        return None;
    }
    {
        let entry = security.entry.borrow();
        if entry.primary.is_empty() {
            security.set_error(msg!(EnterMasterPassword));
            return Some(PasswordField::Primary);
        }
        if kind == PasswordDialogKind::SetupProtection
            && entry.primary.as_str() != entry.confirmation.as_str()
        {
            drop(entry);
            let mut entry = security.entry.borrow_mut();
            entry.confirmation.zeroize();
            entry.active = PasswordField::Confirmation;
            drop(entry);
            security.set_error(msg!(PasswordsMismatch));
            security.entry_revision.update(|value| *value += 1);
            return Some(PasswordField::Confirmation);
        }
    }

    let keeps_dialog_open = matches!(
        kind,
        PasswordDialogKind::ExistingProtection
            | PasswordDialogKind::Unlock { .. }
            | PasswordDialogKind::UnlockForRecovery { .. }
    );
    if keeps_dialog_open {
        security.busy.set(true);
        security.set_status(msg!(CheckingPassword));
        security.entry_revision.update(|value| *value += 1);
    }

    let password = {
        let mut entry = security.entry.borrow_mut();
        let password = entry.take_primary();
        entry.confirmation.zeroize();
        password
    };
    let master_password = MasterPassword::new(password);
    let outcome = match kind {
        PasswordDialogKind::SetupProtection | PasswordDialogKind::ExistingProtection => {
            match model.borrow_mut().protect_selected(Some(master_password)) {
                SecurityActionOutcome::Completed => PasswordSubmitOutcome::Accepted {
                    schedule_persistence: false,
                    close_dialog: true,
                },
                SecurityActionOutcome::Pending => PasswordSubmitOutcome::Accepted {
                    schedule_persistence: true,
                    close_dialog: kind == PasswordDialogKind::SetupProtection,
                },
                SecurityActionOutcome::AuthenticationFailed => {
                    PasswordSubmitOutcome::AuthenticationFailed
                }
                SecurityActionOutcome::OperationFailed => PasswordSubmitOutcome::OperationFailed,
            }
        }
        PasswordDialogKind::Unlock { note_index } => {
            match model
                .borrow_mut()
                .unlock_note(note_index, master_password, false)
            {
                UnlockOutcome::Pending => PasswordSubmitOutcome::Accepted {
                    schedule_persistence: true,
                    close_dialog: false,
                },
                UnlockOutcome::AuthenticationFailed => PasswordSubmitOutcome::AuthenticationFailed,
                UnlockOutcome::OperationFailed => PasswordSubmitOutcome::OperationFailed,
            }
        }
        PasswordDialogKind::UnlockForRecovery { note_index } => {
            let unlock = model
                .borrow_mut()
                .unlock_note(note_index, master_password, true);
            recovery_password_outcome(unlock)
        }
    };
    let focus = match outcome {
        PasswordSubmitOutcome::Accepted {
            schedule_persistence,
            close_dialog,
        } => {
            if close_dialog {
                security.close();
            }
            if schedule_persistence {
                schedule_autosave(model.clone(), revision);
            }
            None
        }
        PasswordSubmitOutcome::AuthenticationFailed => {
            security.authentication_failed();
            Some(PasswordField::Primary)
        }
        PasswordSubmitOutcome::OperationFailed => {
            security.close();
            None
        }
    };
    revision.update(|value| *value += 1);
    focus
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarGroupToggle {
    Opened,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CategoryDropPosition {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NoteSort {
    field: NoteSortField,
    direction: SortDirection,
}

#[derive(Clone, Copy)]
struct CategorySortPopoverSignals {
    sidebar_state: RwSignal<SidebarState>,
    revision: RwSignal<u64>,
    open: RwSignal<bool>,
    field: RwSignal<NoteSortField>,
    direction: RwSignal<SortDirection>,
}

#[derive(Clone, Copy)]
struct SidebarNoteSignals {
    sidebar_state: RwSignal<SidebarState>,
    note_drag: RwSignal<NoteDragState>,
    revision: RwSignal<u64>,
}

impl Default for NoteSort {
    fn default() -> Self {
        Self {
            field: NoteSortField::Name,
            direction: SortDirection::Ascending,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SidebarState {
    collapsed: bool,
    expanded: HashSet<SidebarFilter>,
    creation_group: SidebarFilter,
    category_order: Vec<String>,
    note_sort: BTreeMap<String, NoteSort>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            collapsed: false,
            expanded: HashSet::from([SidebarFilter::All]),
            creation_group: SidebarFilter::All,
            category_order: Vec::new(),
            note_sort: BTreeMap::new(),
        }
    }
}

impl SidebarState {
    fn from_settings<'a>(
        settings: &SidebarSettings,
        categories: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let categories = categories.into_iter().collect::<Vec<_>>();
        let mut state = Self {
            collapsed: settings.collapsed,
            expanded: settings
                .expanded
                .iter()
                .cloned()
                .map(SidebarFilter::from)
                .collect(),
            creation_group: SidebarFilter::from(settings.creation_group.clone()),
            category_order: settings.category_order.clone(),
            note_sort: settings
                .note_sort
                .iter()
                .map(|sort| {
                    (
                        sort.category.clone(),
                        NoteSort {
                            field: sort.field,
                            direction: sort.direction,
                        },
                    )
                })
                .collect(),
        };
        state.reconcile_categories(categories);
        state
    }

    fn to_settings(&self, width: f64) -> SidebarSettings {
        let mut expanded = self
            .expanded
            .iter()
            .cloned()
            .map(PersistedSidebarGroup::from)
            .collect::<Vec<_>>();
        expanded.sort();
        SidebarSettings {
            collapsed: self.collapsed,
            width,
            expanded,
            creation_group: PersistedSidebarGroup::from(self.creation_group.clone()),
            category_order: self.category_order.clone(),
            note_sort: self
                .note_sort
                .iter()
                .map(|(category, sort)| CategoryNoteSortSettings {
                    category: category.clone(),
                    field: sort.field,
                    direction: sort.direction,
                })
                .collect(),
        }
    }

    fn is_expanded(&self, filter: &SidebarFilter) -> bool {
        self.expanded.contains(filter)
    }

    fn toggle_group(&mut self, filter: SidebarFilter) -> SidebarGroupToggle {
        if self.expanded.remove(&filter) {
            let closes_creation_group = match (&filter, &self.creation_group) {
                (SidebarFilter::Tag(parent), SidebarFilter::Tag(active)) => {
                    category_path_is_same_or_descendant(active, parent)
                }
                _ => self.creation_group == filter,
            };
            if closes_creation_group {
                self.creation_group = SidebarFilter::All;
            }
            SidebarGroupToggle::Closed
        } else {
            self.expanded.insert(filter.clone());
            if filter != SidebarFilter::Trash {
                self.creation_group = filter;
            }
            SidebarGroupToggle::Opened
        }
    }

    fn use_group(&mut self, filter: SidebarFilter) {
        if filter != SidebarFilter::Trash {
            self.creation_group = filter;
        }
    }

    fn reconcile_categories<'a>(&mut self, categories: impl IntoIterator<Item = &'a str>) {
        let categories = categories.into_iter().collect::<Vec<_>>();
        let category_paths = sidebar_category_paths(categories.iter().copied());
        self.expanded.retain(|filter| match filter {
            SidebarFilter::All | SidebarFilter::Favorites | SidebarFilter::Trash => true,
            SidebarFilter::Tag(tag) => category_paths.contains(tag),
        });
        if matches!(
            &self.creation_group,
            SidebarFilter::Tag(tag) if !category_paths.contains(tag)
        ) {
            self.creation_group = SidebarFilter::All;
        }
        self.category_order =
            reconciled_category_order(categories.iter().copied(), &self.category_order);
        self.note_sort.retain(|category, _| {
            category == FAVORITED_ORDER_KEY || category_paths.contains(category)
        });
    }

    fn reorder_category(
        &mut self,
        source: &str,
        target: &str,
        position: CategoryDropPosition,
    ) -> bool {
        if source == target || category_parent_path(source) != category_parent_path(target) {
            return false;
        }
        let source_block = self
            .category_order
            .iter()
            .filter(|path| category_path_is_same_or_descendant(path, source))
            .cloned()
            .collect::<Vec<_>>();
        if source_block.is_empty() || !self.category_order.iter().any(|path| path == target) {
            return false;
        }
        let mut reordered = self
            .category_order
            .iter()
            .filter(|path| !category_path_is_same_or_descendant(path, source))
            .cloned()
            .collect::<Vec<_>>();
        let Some(target_index) = reordered.iter().position(|path| path == target) else {
            return false;
        };
        let insertion_index = match position {
            CategoryDropPosition::Before => target_index,
            CategoryDropPosition::After => reordered
                .iter()
                .enumerate()
                .skip(target_index + 1)
                .find_map(|(index, path)| {
                    (!category_path_is_same_or_descendant(path, target)).then_some(index)
                })
                .unwrap_or(reordered.len()),
        };
        reordered.splice(insertion_index..insertion_index, source_block);
        if reordered == self.category_order {
            return false;
        }
        self.category_order = reordered;
        true
    }

    fn note_sort(&self, category: &str) -> NoteSort {
        self.note_sort.get(category).copied().unwrap_or_default()
    }

    fn set_note_sort(&mut self, category: String, sort: NoteSort) {
        if sort == NoteSort::default() {
            self.note_sort.remove(&category);
        } else {
            self.note_sort.insert(category, sort);
        }
    }

    fn use_manual_note_order(&mut self, category: &str) {
        self.note_sort.remove(category);
    }
}

impl From<PersistedSidebarGroup> for SidebarFilter {
    fn from(group: PersistedSidebarGroup) -> Self {
        match group {
            PersistedSidebarGroup::All => Self::All,
            PersistedSidebarGroup::Favorites => Self::Favorites,
            PersistedSidebarGroup::Tag(tag) => Self::Tag(tag),
            PersistedSidebarGroup::Trash => Self::Trash,
        }
    }
}

impl From<SidebarFilter> for PersistedSidebarGroup {
    fn from(filter: SidebarFilter) -> Self {
        match filter {
            SidebarFilter::All => Self::All,
            SidebarFilter::Favorites => Self::Favorites,
            SidebarFilter::Tag(tag) => Self::Tag(tag),
            SidebarFilter::Trash => Self::Trash,
        }
    }
}

struct NotePointerDragView {
    id: ViewId,
    origin: Option<Point>,
    active: bool,
    on_click: Box<dyn Fn()>,
    on_drag: Box<dyn Fn(f64)>,
    on_drop: Box<dyn Fn()>,
    on_cancel: Box<dyn Fn()>,
}

impl NotePointerDragView {
    fn new(
        child: impl IntoView,
        on_click: impl Fn() + 'static,
        on_drag: impl Fn(f64) + 'static,
        on_drop: impl Fn() + 'static,
        on_cancel: impl Fn() + 'static,
    ) -> Self {
        let id = ViewId::new();
        id.add_child(Box::new(child.into_view()));
        Self {
            id,
            origin: None,
            active: false,
            on_click: Box::new(on_click),
            on_drag: Box::new(on_drag),
            on_drop: Box::new(on_drop),
            on_cancel: Box::new(on_cancel),
        }
    }
}

impl View for NotePointerDragView {
    fn id(&self) -> ViewId {
        self.id
    }

    fn event_before_children(
        &mut self,
        _cx: &mut floem::context::EventCx,
        event: &Event,
    ) -> EventPropagation {
        match event {
            Event::PointerDown(pointer) if pointer.button.is_primary() => {
                self.origin = Some(pointer.pos);
                self.active = false;
                self.id.request_focus();
                self.id.request_active();
                EventPropagation::Stop
            }
            Event::PointerMove(pointer) => {
                let Some(origin) = self.origin else {
                    return EventPropagation::Continue;
                };
                let delta_y = pointer.pos.y - origin.y;
                if !self.active && category_drag_threshold_reached(origin, pointer.pos) {
                    self.active = true;
                }
                if self.active {
                    (self.on_drag)(delta_y);
                }
                EventPropagation::Stop
            }
            Event::PointerUp(pointer) if pointer.button.is_primary() && self.origin.is_some() => {
                self.origin = None;
                if self.active {
                    self.active = false;
                    (self.on_drop)();
                } else {
                    (self.on_cancel)();
                    (self.on_click)();
                }
                EventPropagation::Stop
            }
            Event::PointerLeave if self.origin.is_none() => {
                (self.on_cancel)();
                EventPropagation::Continue
            }
            _ => EventPropagation::Continue,
        }
    }
}

fn is_search_shortcut(key_event: &floem::keyboard::KeyEvent) -> bool {
    let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
        && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
    shortcut
        && matches!(
            &key_event.key.logical_key,
            Key::Character(character) if character.eq_ignore_ascii_case("k")
        )
}

fn is_note_find_shortcut(key_event: &floem::keyboard::KeyEvent) -> bool {
    let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
        && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
    shortcut
        && matches!(
            &key_event.key.logical_key,
            Key::Character(character) if character.eq_ignore_ascii_case("f")
        )
}

fn is_go_to_line_shortcut(key_event: &floem::keyboard::KeyEvent) -> bool {
    let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
        && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
    shortcut
        && matches!(
            &key_event.key.logical_key,
            Key::Character(character) if character.eq_ignore_ascii_case("l")
        )
}

fn document_is_open(model: &Rc<RefCell<AppModel>>) -> bool {
    model
        .borrow()
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .is_some()
}

fn local_search_is_available(model: &Rc<RefCell<AppModel>>) -> bool {
    model
        .borrow()
        .workspace
        .as_ref()
        .is_some_and(WorkspaceSession::selected_document_supports_local_search)
}

fn protection_action_state(model: &AppModel) -> ProtectionActionState {
    if model.pending_security_action.is_some() {
        return ProtectionActionState::None;
    }
    let decrypting = matches!(
        model.secure_ui_operation.as_ref(),
        Some(
            SecureUiOperation::Unlock { .. }
                | SecureUiOperation::OpenProtected
                | SecureUiOperation::DisableProtection
        )
    );
    if model.secure_worker_active && !decrypting {
        return ProtectionActionState::None;
    }
    let Some(workspace) = model.workspace.as_ref() else {
        return ProtectionActionState::None;
    };
    if workspace.secure_operation_pending() && !decrypting {
        return ProtectionActionState::None;
    }
    let Some(note_index) = workspace.selected_note() else {
        return ProtectionActionState::None;
    };
    let Some(note) = workspace.notes().get(note_index) else {
        return ProtectionActionState::None;
    };
    if decrypting {
        return if note.protection == NoteProtection::Protected {
            ProtectionActionState::Decrypting
        } else {
            ProtectionActionState::None
        };
    }
    match note.protection {
        NoteProtection::Plain => ProtectionActionState::Protect,
        NoteProtection::Protected if workspace.document().is_some() => ProtectionActionState::Lock,
        NoteProtection::Protected if workspace.has_master_password() => {
            ProtectionActionState::UnlockKnown { note_index }
        }
        NoteProtection::Protected => ProtectionActionState::Unlock { note_index },
    }
}

fn protection_password_dialog(workspace: &WorkspaceSession) -> PasswordDialogKind {
    if workspace.master_password_configured() || workspace.has_protected_notes() {
        PasswordDialogKind::ExistingProtection
    } else {
        PasswordDialogKind::SetupProtection
    }
}

fn selected_note_is_ready(model: &Rc<RefCell<AppModel>>) -> bool {
    selected_note_flag(model, |note| note.availability.is_ready())
}

fn displayed_sidebar_width(saved: f64, window: f64, collapsed: bool) -> f64 {
    if collapsed {
        return 56.0;
    }
    if window < 1000.0 {
        return 200.0;
    }
    saved
        .clamp(SIDEBAR_MIN_WIDTH_PX, SIDEBAR_MAX_WIDTH_PX)
        .min(window * 0.4)
}

fn resized_sidebar_width(current_width: f64, pointer_x: f64, grab_x: f64) -> f64 {
    (current_width + pointer_x - grab_x).clamp(SIDEBAR_MIN_WIDTH_PX, SIDEBAR_MAX_WIDTH_PX)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TagSuggestionDirection {
    Previous,
    Next,
}

fn tag_suggestions<'a>(
    categories: impl IntoIterator<Item = &'a str>,
    assigned: &[String],
    query: &str,
) -> Vec<String> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }
    categories
        .into_iter()
        .filter(|category| !assigned.iter().any(|tag| tag == category))
        .filter(|category| category.to_lowercase().starts_with(&normalized))
        .map(str::to_owned)
        .collect()
}

fn move_tag_suggestion_highlight(
    current: Option<usize>,
    suggestion_count: usize,
    direction: TagSuggestionDirection,
) -> Option<usize> {
    if suggestion_count == 0 {
        return None;
    }
    match (current, direction) {
        (None, TagSuggestionDirection::Next) => Some(0),
        (None, TagSuggestionDirection::Previous) => Some(suggestion_count - 1),
        (Some(index), TagSuggestionDirection::Next) => {
            Some(index.saturating_add(1).min(suggestion_count - 1))
        }
        (Some(index), TagSuggestionDirection::Previous) => {
            Some(index.min(suggestion_count - 1).saturating_sub(1))
        }
    }
}

fn tag_submission(
    query: &str,
    suggestions: &[String],
    highlighted: Option<usize>,
) -> Option<String> {
    highlighted
        .and_then(|index| suggestions.get(index).cloned())
        .or_else(|| {
            let trimmed = query.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
}

fn category_parent_path(category: &str) -> Option<String> {
    let segments = category_path_segments(category);
    (segments.len() > 1).then(|| segments[..segments.len() - 1].join("/"))
}

fn sidebar_category_paths<'a>(categories: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    let mut paths = HashSet::new();
    for category in categories {
        let mut path = String::new();
        for segment in category_path_segments(category) {
            if path.is_empty() {
                path.push_str(segment);
            } else {
                path.push('/');
                path.push_str(segment);
            }
            paths.insert(path.clone());
        }
    }
    paths
}

fn matching_tag_indices<'a>(
    notes: impl IntoIterator<Item = (&'a [String], bool, bool)>,
    filter: &SidebarFilter,
) -> Vec<usize> {
    notes
        .into_iter()
        .enumerate()
        .filter_map(|(index, (tags, favorited, deleted))| {
            note_matches_filter(tags, favorited, deleted, filter).then_some(index)
        })
        .collect()
}

#[derive(Debug)]
struct SidebarCategoryBuilder {
    path: String,
    label: String,
    children: BTreeMap<String, SidebarCategoryBuilder>,
}

impl SidebarCategoryBuilder {
    fn new(path: String, label: String) -> Self {
        Self {
            path,
            label,
            children: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SidebarCategoryNode {
    path: String,
    label: String,
    direct_notes: Vec<usize>,
    subtree_notes: Vec<usize>,
    children: Vec<SidebarCategoryNode>,
}

fn insert_sidebar_category(roots: &mut BTreeMap<String, SidebarCategoryBuilder>, category: &str) {
    let mut path = String::new();
    let mut children = roots;
    for segment in category_path_segments(category) {
        if path.is_empty() {
            path.push_str(segment);
        } else {
            path.push('/');
            path.push_str(segment);
        }
        let node = children
            .entry(segment.to_owned())
            .or_insert_with(|| SidebarCategoryBuilder::new(path.clone(), segment.to_owned()));
        children = &mut node.children;
    }
}

fn finish_sidebar_category(
    builder: SidebarCategoryBuilder,
    notes: &[(&[String], bool, bool)],
    order: &HashMap<&str, usize>,
) -> SidebarCategoryNode {
    let mut children = builder
        .children
        .into_values()
        .map(|child| finish_sidebar_category(child, notes, order))
        .collect::<Vec<_>>();
    children.sort_by(|left, right| {
        category_order_key(&left.path, order).cmp(&category_order_key(&right.path, order))
    });
    let direct_notes = notes
        .iter()
        .enumerate()
        .filter_map(|(index, (tags, _, deleted))| {
            (!deleted && tags.iter().any(|tag| tag == &builder.path)).then_some(index)
        })
        .collect::<Vec<_>>();
    let mut subtree_notes = direct_notes.iter().copied().collect::<BTreeSet<_>>();
    for child in &children {
        subtree_notes.extend(child.subtree_notes.iter().copied());
    }
    SidebarCategoryNode {
        path: builder.path,
        label: builder.label,
        direct_notes,
        subtree_notes: subtree_notes.into_iter().collect(),
        children,
    }
}

fn sidebar_category_tree<'a>(
    notes: &[(&[String], bool, bool)],
    categories: impl IntoIterator<Item = &'a str>,
    category_order: &[String],
) -> Vec<SidebarCategoryNode> {
    let mut roots = BTreeMap::new();
    for category in categories {
        insert_sidebar_category(&mut roots, category);
    }
    let order = category_order
        .iter()
        .enumerate()
        .map(|(index, path)| (path.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut roots = roots
        .into_values()
        .map(|root| finish_sidebar_category(root, notes, &order))
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        category_order_key(&left.path, &order).cmp(&category_order_key(&right.path, &order))
    });
    roots
}

fn category_order_key<'a>(path: &'a str, order: &HashMap<&str, usize>) -> (bool, usize, &'a str) {
    match order.get(path) {
        Some(index) => (true, *index, path),
        None => (false, 0, path),
    }
}

fn flatten_category_paths(nodes: &[SidebarCategoryNode], paths: &mut Vec<String>) {
    for node in nodes {
        paths.push(node.path.clone());
        flatten_category_paths(&node.children, paths);
    }
}

fn reconciled_category_order<'a>(
    categories: impl IntoIterator<Item = &'a str>,
    category_order: &[String],
) -> Vec<String> {
    let tree = sidebar_category_tree(&[], categories, category_order);
    let mut paths = Vec::new();
    flatten_category_paths(&tree, &mut paths);
    paths
}

/// One visible row of the navigation tree in the single sidebar.
#[derive(Clone, Debug, Eq, PartialEq)]
enum SidebarRow {
    ExternalGroup {
        count: usize,
    },
    ExternalFile {
        index: usize,
    },
    /// `Все`, `Избранное` or a derived category with its item count.
    Group {
        filter: SidebarFilter,
        title: String,
        count: usize,
        depth: usize,
    },
    /// A note listed inline under one expanded group; `index` is the workspace
    /// note index. The parent keeps duplicate appearances uniquely keyed.
    Note {
        parent: SidebarFilter,
        index: usize,
        depth: usize,
    },
    Engine {
        parent: SidebarFilter,
        index: usize,
        depth: usize,
    },
    /// Visual break between `Избранное` and the derived categories.
    Separator,
}

fn push_sidebar_group(
    rows: &mut Vec<SidebarRow>,
    filter: SidebarFilter,
    title: &str,
    matching: Vec<usize>,
    state: &SidebarState,
) {
    let expanded = state.is_expanded(&filter);
    rows.push(SidebarRow::Group {
        filter: filter.clone(),
        title: title.to_owned(),
        count: matching.len(),
        depth: 0,
    });
    if expanded {
        rows.extend(matching.into_iter().map(|index| SidebarRow::Note {
            parent: filter.clone(),
            index,
            depth: 0,
        }));
    }
}

fn push_sidebar_category(
    rows: &mut Vec<SidebarRow>,
    category: &SidebarCategoryNode,
    depth: usize,
    state: &SidebarState,
) {
    let filter = SidebarFilter::Tag(category.path.clone());
    let expanded = state.is_expanded(&filter);
    rows.push(SidebarRow::Group {
        filter: filter.clone(),
        title: category.label.clone(),
        count: category.subtree_notes.len(),
        depth,
    });
    if !expanded {
        return;
    }
    for child in &category.children {
        push_sidebar_category(rows, child, depth.saturating_add(1), state);
    }
    rows.extend(
        category
            .direct_notes
            .iter()
            .copied()
            .map(|index| SidebarRow::Note {
                parent: filter.clone(),
                index,
                depth,
            }),
    );
}

/// Flatten special roots and the recursively expanded category tree.
fn sidebar_rows<'a>(
    notes: &[(&'a [String], bool, bool)],
    categories: impl IntoIterator<Item = &'a str>,
    state: &SidebarState,
) -> Vec<SidebarRow> {
    let categories = sidebar_category_tree(notes, categories, &state.category_order);
    let mut rows = Vec::new();
    let favorites = SidebarFilter::Favorites;
    push_sidebar_group(
        &mut rows,
        favorites.clone(),
        &tr!(Favorites),
        matching_tag_indices(notes.iter().copied(), &favorites),
        state,
    );
    if !categories.is_empty() {
        rows.push(SidebarRow::Separator);
    }
    for category in &categories {
        push_sidebar_category(&mut rows, category, 0, state);
    }
    let all = SidebarFilter::All;
    push_sidebar_group(
        &mut rows,
        all.clone(),
        &tr!(All),
        matching_tag_indices(notes.iter().copied(), &all),
        state,
    );
    let trash = SidebarFilter::Trash;
    push_sidebar_group(
        &mut rows,
        trash.clone(),
        &tr!(Trash),
        matching_tag_indices(notes.iter().copied(), &trash),
        state,
    );
    rows
}

fn current_sidebar_rows(model: &AppModel, state: &SidebarState) -> Vec<SidebarRow> {
    let Some(workspace) = model.workspace.as_ref() else {
        return Vec::new();
    };
    let rss = workspace.non_document_items();
    let note_count = workspace.notes().len();
    let mut projected = workspace
        .notes()
        .iter()
        .map(|note| (note.tags.as_slice(), note.favorited, note.deleted))
        .collect::<Vec<_>>();
    projected.extend(rss.iter().map(|summary| {
        let subscription = &summary.metadata;
        (
            subscription.categories.as_slice(),
            subscription.favorited,
            subscription.deleted,
        )
    }));
    let categories = workspace
        .categories()
        .iter()
        .map(|category| category.name.as_str());
    let mut rows = Vec::new();
    if !workspace.external_files().is_empty() {
        rows.push(SidebarRow::ExternalGroup {
            count: workspace.external_files().len(),
        });
        rows.extend(
            (0..workspace.external_files().len()).map(|index| SidebarRow::ExternalFile { index }),
        );
        rows.push(SidebarRow::Separator);
    }
    rows.extend(
        sidebar_rows(&projected, categories, state)
            .into_iter()
            .map(|row| match row {
                SidebarRow::Note {
                    parent,
                    index,
                    depth,
                } if index >= note_count => SidebarRow::Engine {
                    parent,
                    index: index - note_count,
                    depth,
                },
                row => row,
            }),
    );
    sort_sidebar_catalog_rows(&mut rows, workspace.notes(), &rss, state);
    rows
}

#[derive(Clone, Copy)]
enum CatalogRowIndex {
    Note(usize),
    Engine(usize),
}

fn catalog_pinned(
    index: CatalogRowIndex,
    notes: &[stillus_core::NoteSummary],
    rss: &[stillus_engine::ItemSummary],
) -> bool {
    match index {
        CatalogRowIndex::Note(index) => notes[index].pinned,
        CatalogRowIndex::Engine(index) => rss[index].metadata.pinned,
    }
}

fn catalog_title<'a>(
    index: CatalogRowIndex,
    notes: &'a [stillus_core::NoteSummary],
    rss: &'a [stillus_engine::ItemSummary],
) -> &'a str {
    match index {
        CatalogRowIndex::Note(index) => &notes[index].title,
        CatalogRowIndex::Engine(index) => &rss[index].metadata.title,
    }
}

fn catalog_date<'a>(
    index: CatalogRowIndex,
    field: NoteSortField,
    notes: &'a [stillus_core::NoteSummary],
    rss: &'a [stillus_engine::ItemSummary],
) -> Option<&'a str> {
    match (index, field) {
        (CatalogRowIndex::Note(index), NoteSortField::Created) => notes[index].created.as_deref(),
        (CatalogRowIndex::Note(index), NoteSortField::Modified) => notes[index].modified.as_deref(),
        (CatalogRowIndex::Engine(index), NoteSortField::Created) => {
            rss[index].metadata.created.as_deref()
        }
        (CatalogRowIndex::Engine(index), NoteSortField::Modified) => {
            rss[index].metadata.modified.as_deref()
        }
        (_, NoteSortField::Name) => None,
    }
}

fn catalog_order_rank<'a>(
    index: CatalogRowIndex,
    key: &str,
    notes: &'a [stillus_core::NoteSummary],
    rss: &'a [stillus_engine::ItemSummary],
) -> Option<&'a u32> {
    match index {
        CatalogRowIndex::Note(index) => notes[index].order.get(key),
        CatalogRowIndex::Engine(index) => rss[index].metadata.order.get(key),
    }
}

fn catalog_row_order(
    left: CatalogRowIndex,
    right: CatalogRowIndex,
    key: &str,
    sort: NoteSort,
    manual: bool,
    notes: &[stillus_core::NoteSummary],
    rss: &[stillus_engine::ItemSummary],
) -> Ordering {
    let partition = catalog_pinned(right, notes, rss).cmp(&catalog_pinned(left, notes, rss));
    if partition != Ordering::Equal {
        return partition;
    }
    let title_order = || {
        let left_title = catalog_title(left, notes, rss);
        let right_title = catalog_title(right, notes, rss);
        left_title
            .to_lowercase()
            .cmp(&right_title.to_lowercase())
            .then_with(|| left_title.cmp(right_title))
    };
    let primary = if manual {
        match (
            catalog_order_rank(left, key, notes, rss),
            catalog_order_rank(right, key, notes, rss),
        ) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    } else if sort.field == NoteSortField::Name {
        match sort.direction {
            SortDirection::Ascending => title_order(),
            SortDirection::Descending => title_order().reverse(),
        }
    } else {
        optional_date_order(
            catalog_date(left, sort.field, notes, rss),
            catalog_date(right, sort.field, notes, rss),
            sort.direction,
        )
    };
    primary.then_with(title_order)
}

fn sort_sidebar_catalog_rows(
    rows: &mut [SidebarRow],
    notes: &[stillus_core::NoteSummary],
    rss: &[stillus_engine::ItemSummary],
    state: &SidebarState,
) {
    let mut groups = HashMap::<SidebarFilter, Vec<(usize, CatalogRowIndex)>>::new();
    for (position, row) in rows.iter().enumerate() {
        let pair = match row {
            SidebarRow::Note { parent, index, .. } => Some((parent, CatalogRowIndex::Note(*index))),
            SidebarRow::Engine { parent, index, .. } => {
                Some((parent, CatalogRowIndex::Engine(*index)))
            }
            _ => None,
        };
        if let Some((parent, index)) = pair
            && sidebar_note_order_key(parent).is_some()
        {
            groups
                .entry(parent.clone())
                .or_default()
                .push((position, index));
        }
    }
    for (group, entries) in groups {
        let key = sidebar_note_order_key(&group).expect("sortable catalog group has an order key");
        let manual = entries
            .iter()
            .any(|(_, index)| catalog_order_rank(*index, key, notes, rss).is_some());
        let sort = state.note_sort(key);
        let mut indices = entries.iter().map(|(_, index)| *index).collect::<Vec<_>>();
        indices
            .sort_by(|left, right| catalog_row_order(*left, *right, key, sort, manual, notes, rss));
        for ((position, _), index) in entries.into_iter().zip(indices) {
            let (parent, depth) = match &rows[position] {
                SidebarRow::Note { parent, depth, .. }
                | SidebarRow::Engine { parent, depth, .. } => (parent.clone(), *depth),
                _ => unreachable!("catalog row position changed while sorting"),
            };
            rows[position] = match index {
                CatalogRowIndex::Note(index) => SidebarRow::Note {
                    parent,
                    index,
                    depth,
                },
                CatalogRowIndex::Engine(index) => SidebarRow::Engine {
                    parent,
                    index,
                    depth,
                },
            };
        }
    }
}

fn optional_date_order(
    left: Option<&str>,
    right: Option<&str>,
    direction: SortDirection,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            SortDirection::Ascending => left.cmp(right),
            SortDirection::Descending => right.cmp(left),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn ordered_sidebar_catalog_items(
    model: &AppModel,
    state: &SidebarState,
    group: &SidebarFilter,
) -> Vec<(CatalogOrderItem, bool)> {
    let Some(workspace) = model.workspace.as_ref() else {
        return Vec::new();
    };
    current_sidebar_rows(model, state)
        .into_iter()
        .filter_map(|row| match row {
            SidebarRow::Note { parent, index, .. } if &parent == group => workspace
                .notes()
                .get(index)
                .map(|note| (CatalogOrderItem::Note(note.path.clone()), note.pinned)),
            SidebarRow::Engine { parent, index, .. } if &parent == group => {
                workspace.non_document_items().get(index).map(|summary| {
                    (
                        CatalogOrderItem::Engine(
                            summary.engine_id.clone(),
                            summary.item_id.clone(),
                        ),
                        summary.metadata.pinned,
                    )
                })
            }
            _ => None,
        })
        .collect()
}

fn note_drop_target(
    siblings: &[(CatalogOrderItem, bool)],
    source: &CatalogOrderItem,
    pinned: bool,
    delta_y: f64,
) -> Option<(CatalogOrderItem, CategoryDropPosition)> {
    let partition = siblings
        .iter()
        .filter(|(_, candidate_pinned)| *candidate_pinned == pinned)
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    let source_index = partition.iter().position(|item| *item == source)?;
    let offset = (delta_y / (SIDEBAR_NOTE_ROW_HEIGHT_PX + SIDEBAR_TREE_ROW_GAP_PX)).round();
    let target_index = (source_index as f64 + offset)
        .clamp(0.0, partition.len().saturating_sub(1) as f64) as usize;
    if target_index == source_index {
        return None;
    }
    Some((
        partition[target_index].clone(),
        if target_index < source_index {
            CategoryDropPosition::Before
        } else {
            CategoryDropPosition::After
        },
    ))
}

fn reordered_catalog_items(
    siblings: &[(CatalogOrderItem, bool)],
    source: &CatalogOrderItem,
    target: &CatalogOrderItem,
    position: CategoryDropPosition,
) -> Option<Vec<CatalogOrderItem>> {
    let mut items = siblings
        .iter()
        .map(|(item, _)| item.clone())
        .collect::<Vec<_>>();
    let source_index = items.iter().position(|item| item == source)?;
    let source = items.remove(source_index);
    let target_index = items.iter().position(|item| item == target)?;
    let insertion = match position {
        CategoryDropPosition::Before => target_index,
        CategoryDropPosition::After => target_index + 1,
    };
    items.insert(insertion, source);
    Some(items)
}

/// Keyed identity of a tree row: metadata that changes how the row renders is
/// part of the key, so Floem rebuilds exactly the rows whose content changed.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SidebarRowKey {
    ExternalGroup {
        count: usize,
    },
    ExternalFile {
        path: PathBuf,
        title: String,
        ready: bool,
    },
    Group {
        filter: SidebarFilter,
        title: String,
        count: usize,
        depth: usize,
    },
    Note {
        parent: SidebarFilter,
        depth: usize,
        path: PathBuf,
        title: String,
        pinned: bool,
        favorited: bool,
        ready: bool,
        protected: bool,
    },
    Engine {
        parent: SidebarFilter,
        depth: usize,
        id: ItemId,
        engine: stillus_engine::EngineId,
        title: String,
        unread: u64,
        pinned: bool,
        favorited: bool,
        deleted: bool,
    },
    Separator,
}

#[derive(Clone)]
enum SidebarItem {
    ExternalGroup {
        count: usize,
    },
    ExternalFile(ExternalFileSummary),
    Group {
        filter: SidebarFilter,
        title: String,
        count: usize,
        depth: usize,
    },
    Note {
        parent: SidebarFilter,
        depth: usize,
        note: stillus_core::NoteSummary,
    },
    Engine {
        parent: SidebarFilter,
        depth: usize,
        summary: stillus_engine::ItemSummary,
    },
    Separator,
}

impl SidebarItem {
    fn key(&self) -> SidebarRowKey {
        match self {
            SidebarItem::ExternalGroup { count } => SidebarRowKey::ExternalGroup { count: *count },
            SidebarItem::ExternalFile(file) => SidebarRowKey::ExternalFile {
                path: file.path.clone(),
                title: file.title.clone(),
                ready: matches!(file.availability, stillus_core::ItemAvailability::Ready),
            },
            SidebarItem::Group {
                filter,
                title,
                count,
                depth,
            } => SidebarRowKey::Group {
                filter: filter.clone(),
                title: title.clone(),
                count: *count,
                depth: *depth,
            },
            SidebarItem::Note {
                parent,
                depth,
                note,
            } => SidebarRowKey::Note {
                parent: parent.clone(),
                depth: *depth,
                path: note.path.clone(),
                title: note.title.clone(),
                pinned: note.pinned,
                favorited: note.favorited,
                ready: note.availability.is_ready(),
                protected: note.protection == NoteProtection::Protected,
            },
            SidebarItem::Engine {
                parent,
                depth,
                summary,
            } => SidebarRowKey::Engine {
                parent: parent.clone(),
                depth: *depth,
                id: summary.item_id.clone(),
                engine: summary.engine_id.clone(),
                title: summary.metadata.title.clone(),
                unread: summary.badge.unwrap_or(0),
                pinned: summary.metadata.pinned,
                favorited: summary.metadata.favorited,
                deleted: summary.metadata.deleted,
            },
            SidebarItem::Separator => SidebarRowKey::Separator,
        }
    }
}

const SIDEBAR_GROUP_ROW_HEIGHT_PX: f64 = 34.0;
const SIDEBAR_NOTE_ROW_HEIGHT_PX: f64 = 30.0;
const SIDEBAR_SECTION_GAP_PX: f64 = 6.0;
const SIDEBAR_TREE_ROW_GAP_PX: f64 = 2.0;
const SIDEBAR_TREE_INDENT_PX: f64 = 16.0;
const SIDEBAR_TREE_MAX_VISUAL_DEPTH: usize = 6;
const CATEGORY_DRAG_THRESHOLD_PX: f64 = 4.0;
const SIDEBAR_SORT_REGION_WIDTH_PX: f64 = 64.0;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CategoryDragState {
    source: Option<String>,
    target: Option<(String, CategoryDropPosition)>,
}

#[derive(Clone, Debug, PartialEq)]
struct CategoryDragHitRegion {
    path: String,
    top: f64,
    bottom: f64,
}

#[derive(Debug, Default)]
struct CategoryPointerDrag {
    source: Option<String>,
    origin: Option<Point>,
    active: bool,
    hit_regions: Vec<CategoryDragHitRegion>,
}

struct SidebarGroupPointerView {
    id: ViewId,
    on_press: Box<dyn Fn()>,
}

impl SidebarGroupPointerView {
    fn new(child: impl IntoView, on_press: impl Fn() + 'static) -> Self {
        let id = ViewId::new();
        id.add_child(Box::new(child.into_view()));
        Self {
            id,
            on_press: Box::new(on_press),
        }
    }
}

impl View for SidebarGroupPointerView {
    fn id(&self) -> ViewId {
        self.id
    }

    fn event_before_children(
        &mut self,
        _cx: &mut floem::context::EventCx,
        event: &Event,
    ) -> EventPropagation {
        let Event::PointerDown(pointer) = event else {
            return EventPropagation::Continue;
        };
        if !pointer.button.is_primary() {
            return EventPropagation::Continue;
        }
        let width = self.id.get_size().map_or(0.0, |size| size.width);
        if pointer.pos.x >= (width - SIDEBAR_SORT_REGION_WIDTH_PX).max(0.0) {
            return EventPropagation::Continue;
        }
        self.id.request_focus();
        (self.on_press)();
        EventPropagation::Stop
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct NoteDragState {
    source: Option<CatalogOrderItem>,
    target: Option<(CatalogOrderItem, CategoryDropPosition)>,
}

fn sidebar_row_height(row: &SidebarRow) -> f64 {
    match row {
        SidebarRow::ExternalGroup { .. } | SidebarRow::Group { .. } => SIDEBAR_GROUP_ROW_HEIGHT_PX,
        SidebarRow::ExternalFile { .. } | SidebarRow::Note { .. } | SidebarRow::Engine { .. } => {
            SIDEBAR_NOTE_ROW_HEIGHT_PX
        }
        SidebarRow::Separator => SIDEBAR_SECTION_GAP_PX,
    }
}

fn category_drag_hit_regions(rows: &[SidebarRow]) -> Vec<CategoryDragHitRegion> {
    let mut top = 0.0;
    let mut regions = Vec::new();
    for row in rows {
        let bottom = top + sidebar_row_height(row);
        if let SidebarRow::Group {
            filter: SidebarFilter::Tag(path),
            ..
        } = row
        {
            regions.push(CategoryDragHitRegion {
                path: path.clone(),
                top,
                bottom,
            });
        }
        top = bottom + SIDEBAR_TREE_ROW_GAP_PX;
    }
    regions
}

fn category_drag_source_at_point(
    hit_regions: &[CategoryDragHitRegion],
    point: Point,
    width: f64,
) -> Option<String> {
    if point.x < 0.0 || point.x >= (width - SIDEBAR_SORT_REGION_WIDTH_PX).max(0.0) {
        return None;
    }
    hit_regions
        .iter()
        .find(|region| point.y >= region.top && point.y < region.bottom)
        .map(|region| region.path.clone())
}

fn category_drop_target_at_point(
    hit_regions: &[CategoryDragHitRegion],
    source: &str,
    point: Point,
    width: f64,
) -> Option<(String, CategoryDropPosition)> {
    if point.x < 0.0 || point.x >= (width - SIDEBAR_SORT_REGION_WIDTH_PX).max(0.0) {
        return None;
    }
    let region = hit_regions
        .iter()
        .find(|region| point.y >= region.top && point.y < region.bottom)?;
    if region.path == source || category_parent_path(source) != category_parent_path(&region.path) {
        return None;
    }
    let position = if point.y < (region.top + region.bottom) / 2.0 {
        CategoryDropPosition::Before
    } else {
        CategoryDropPosition::After
    };
    Some((region.path.clone(), position))
}

fn category_drag_threshold_reached(origin: Point, current: Point) -> bool {
    (current.x - origin.x).hypot(current.y - origin.y) >= CATEGORY_DRAG_THRESHOLD_PX
}

fn sidebar_tree_indent(depth: usize) -> f64 {
    depth.min(SIDEBAR_TREE_MAX_VISUAL_DEPTH) as f64 * SIDEBAR_TREE_INDENT_PX
}

fn sidebar_note_indicator_icons(
    protected: bool,
    pinned: bool,
    favorited: bool,
) -> Vec<&'static str> {
    [
        (ICON_LOCK, protected),
        (ButtonAction::Pin.icon(), pinned),
        (ButtonAction::Favorite.icon(), favorited),
    ]
    .into_iter()
    .filter_map(|(icon, visible)| visible.then_some(icon))
    .collect()
}

fn show_scrollbar_temporarily(visible: RwSignal<bool>, generation: RwSignal<u64>) {
    generation.update(|value| *value = value.saturating_add(1));
    let expected_generation = generation.get_untracked();
    visible.set(true);
    exec_after(Duration::from_millis(SCROLLBAR_HIDE_MS), move |_| {
        let Some(current_generation) = generation.try_get_untracked() else {
            return;
        };
        if current_generation != expected_generation || visible.try_get_untracked().is_none() {
            return;
        }
        visible.set(false);
    });
}

fn activate_sidebar_group(
    filter: &SidebarFilter,
    model: &Rc<RefCell<AppModel>>,
    sidebar_state: RwSignal<SidebarState>,
    revision: RwSignal<u64>,
) {
    let was_expanded = sidebar_state.get_untracked().is_expanded(filter);
    sidebar_state.update(|state| {
        state.toggle_group(filter.clone());
    });
    let refresh_started = if !was_expanded {
        model
            .borrow_mut()
            .open_first_matching_note_if_unselected(filter)
    } else {
        false
    };
    if refresh_started {
        schedule_rss_poll(model.clone(), revision);
    }
    revision.update(|value| *value += 1);
    schedule_autosave(model.clone(), revision);
}

fn external_file_picker_spec(extensions: Vec<String>) -> Option<FileSpec> {
    if extensions.is_empty() {
        return None;
    }

    // Floem's native dialog contract requires static filter descriptors. The
    // registry belongs to the application session and this view is built once,
    // so promoting this small, bounded extension list matches that lifetime.
    let extensions = extensions
        .into_iter()
        .map(|extension| Box::leak(extension.into_boxed_str()) as &'static str)
        .collect::<Vec<_>>();
    Some(FileSpec {
        name: i18n::static_filter_name(),
        extensions: Box::leak(extensions.into_boxed_slice()),
    })
}

fn creation_popover(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    sidebar_state: RwSignal<SidebarState>,
    open: RwSignal<bool>,
    picker_active: RwSignal<bool>,
    file_spec: Option<FileSpec>,
    palette: Palette,
) -> impl IntoView {
    let rss_mode = create_rw_signal(false);
    let rss_url = create_rw_signal(String::new());
    let rss_error = create_rw_signal(None::<UiText>);
    dyn_container(
        move || rss_mode.get(),
        move |show_rss| {
            if show_rss {
                rss_creation_form(
                    model.clone(),
                    revision,
                    sidebar_state,
                    open,
                    rss_mode,
                    rss_url,
                    rss_error,
                    palette,
                )
                .into_any()
            } else {
                creation_choices(
                    model.clone(),
                    revision,
                    sidebar_state,
                    open,
                    picker_active,
                    file_spec,
                    rss_mode,
                    rss_error,
                    palette,
                )
                .into_any()
            }
        },
    )
    .style(move |style| {
        style
            .width(CREATE_POPOVER_WIDTH_PX)
            .apply_if(rss_mode.get(), |style| {
                style
                    .padding(RSS_FORM_PADDING_PX)
                    .background(palette.paper)
                    .color(palette.ink)
                    .border(1.0)
                    .border_color(palette.divider)
                    .border_radius(8.0)
            })
    })
}

#[allow(clippy::too_many_arguments)]
fn creation_choices(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    sidebar_state: RwSignal<SidebarState>,
    open: RwSignal<bool>,
    picker_active: RwSignal<bool>,
    file_spec: Option<FileSpec>,
    rss_mode: RwSignal<bool>,
    rss_error: RwSignal<Option<UiText>>,
    palette: Palette,
) -> impl IntoView {
    let note_model = model.clone();
    let chat_model = model.clone();
    let file_model = model;
    let file_enabled = file_spec.is_some();
    menu(
        vec![
            MenuEntry::action(
                ICON_NOTE,
                || tr!(Note),
                || true,
                move || {
                    open.set(false);
                    let active = sidebar_state.get_untracked().creation_group;
                    note_model.borrow_mut().request_note_creation(active);
                    revision.update(|value| *value += 1);
                    schedule_autosave(note_model.clone(), revision);
                },
            ),
            MenuEntry::action(
                ICON_FILE,
                || tr!(File),
                move || file_enabled,
                move || {
                    open.set(false);
                    let Some(mut file_spec) = file_spec else {
                        return;
                    };
                    if picker_active.get_untracked() {
                        return;
                    }
                    picker_active.set(true);
                    file_spec.name = i18n::static_filter_name();
                    let options = FileDialogOptions::new()
                        .title(tr!(ChooseExternal))
                        .multi_selection()
                        .allowed_types(vec![file_spec]);
                    let selected_model = file_model.clone();
                    open_file(options, move |selection| {
                        picker_active.set(false);
                        let Some(paths) = selection.map(|file| file.path) else {
                            return;
                        };
                        selected_model.borrow_mut().accept_external_paths(&paths);
                        revision.update(|value| *value += 1);
                        schedule_autosave(selected_model.clone(), revision);
                    });
                },
            ),
            MenuEntry::action(
                ICON_RSS,
                || tr!(RssFeed),
                || true,
                move || {
                    rss_error.set(None);
                    rss_mode.set(true);
                },
            )
            .keep_open(),
            MenuEntry::action(
                ICON_CHAT,
                || tr!(ChatMenu),
                || true,
                move || {
                    open.set(false);
                    let group = sidebar_state.get_untracked().creation_group;
                    let categories = if let SidebarFilter::Tag(category) = &group {
                        vec![category.clone()]
                    } else {
                        Vec::new()
                    };
                    let result = chat_model.borrow_mut().dispatch(
                        application::api::Caller::Ui,
                        application::api::Command::Chat(application::chat::Command::Create {
                            title: tr!(ChatNew),
                            categories,
                            favorited: matches!(group, SidebarFilter::Favorites),
                            open: true,
                        }),
                    );
                    if let Err(error) = result {
                        chat_model.borrow_mut().error = Some(UiText::Failure {
                            details: format!("{error:?}"),
                        });
                    }
                    revision.update(|v| *v += 1);
                },
            ),
        ],
        palette,
    )
}

#[allow(clippy::too_many_arguments)]
fn rss_creation_form(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    sidebar_state: RwSignal<SidebarState>,
    open: RwSignal<bool>,
    rss_mode: RwSignal<bool>,
    rss_url: RwSignal<String>,
    rss_error: RwSignal<Option<UiText>>,
    palette: Palette,
) -> impl IntoView {
    let rss_submit_model = model;
    let submit: Rc<dyn Fn()> = Rc::new(move || {
        let url = rss_url.get_untracked();
        let active = sidebar_state.get_untracked().creation_group;
        let item_id = rss_submit_model.borrow_mut().create_rss(&url, &active);
        if let Some(item_id) = item_id {
            if rss_submit_model.borrow_mut().start_rss_refresh(item_id) {
                schedule_rss_poll(rss_submit_model.clone(), revision);
            }
            rss_url.set(String::new());
            rss_error.set(None);
            rss_mode.set(false);
            open.set(false);
            revision.update(|value| *value = value.saturating_add(1));
            schedule_autosave(rss_submit_model.clone(), revision);
        } else {
            rss_error.set(rss_submit_model.borrow().error.clone());
        }
    });
    let input_submit = submit.clone();
    let input = localized_input::LocalizedInput::example(rss_url, "https://example.com/feed.xml")
        .style(move |style| {
            form_field_style(style, palette, rss_error.get().is_some()).width_full()
        })
        .on_event(EventListener::KeyDown, move |event| {
            let Event::KeyDown(key) = event else {
                return EventPropagation::Continue;
            };
            match &key.key.logical_key {
                Key::Named(NamedKey::Enter) => {
                    input_submit();
                    EventPropagation::Stop
                }
                Key::Named(NamedKey::Escape) => {
                    rss_mode.set(false);
                    rss_error.set(None);
                    open.set(false);
                    EventPropagation::Stop
                }
                _ => EventPropagation::Continue,
            }
        });
    let input_id = input.id();
    exec_after(Duration::from_millis(10), move |_| input_id.request_focus());
    let button_submit = submit;
    let submit_enabled = rss_url;
    let header = h_stack((
        svg(ICON_RSS).style(move |style| {
            style
                .size(14.0, 14.0)
                .color(palette.accent)
                .flex_shrink(0.0)
        }),
        label(move || tr!(RssFeed)).style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .font_weight(floem::text::Weight::SEMIBOLD)
                .color(palette.ink)
                .selectable(false)
        }),
    ))
    .style(|style| style.width_full().items_center().gap(8.0));
    // One status slot carries both the hint and the submission error, so the
    // swap keeps the buttons in place; a long error wraps inside the card
    // instead of running past its edge.
    let status = dyn_container(
        move || rss_error.get(),
        move |message| match message {
            Some(message) => text(message)
                .style(move |style| {
                    style
                        .width_full()
                        .font_size(crate::ui::FONT_CAPTION as f32)
                        .color(palette.danger)
                        .selectable(false)
                })
                .into_any(),
            None => label(move || tr!(FeedLink))
                .style(move |style| {
                    style
                        .width_full()
                        .font_size(crate::ui::FONT_CAPTION as f32)
                        .color(palette.muted)
                        .selectable(false)
                })
                .into_any(),
        },
    )
    .style(|style| {
        style
            .width_full()
            .min_height(RSS_FORM_STATUS_HEIGHT_PX)
            .items_center()
    });
    let footer = h_stack((
        content_button(
            ButtonAction::Back.icon(),
            label(move || tr!(Back)).style(|style| {
                style
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .selectable(false)
            }),
            move || {
                rss_mode.set(false);
                rss_error.set(None);
            },
        )
        .style(move |style| {
            style
                .height(RSS_FORM_BUTTON_HEIGHT_PX)
                .padding_horiz(12.0)
                .items_center()
                .justify_center()
                .cursor(CursorStyle::Pointer)
                .background(palette.paper)
                .color(palette.muted)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(6.0)
                .hover(move |style| style.background(palette.canvas).color(palette.ink))
        }),
        empty().style(|style| style.flex_grow(1.0)),
        content_button(
            ButtonAction::Add.icon(),
            label(move || tr!(Add)).style(|style| {
                style
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .selectable(false)
            }),
            move || button_submit(),
        )
        .disabled(move || submit_enabled.get().trim().is_empty())
        .style(move |style| {
            style
                .height(RSS_FORM_BUTTON_HEIGHT_PX)
                .padding_horiz(14.0)
                .items_center()
                .justify_center()
                .cursor(CursorStyle::Pointer)
                .background(palette.accent)
                .color(palette.paper)
                .border(1.0)
                .border_color(palette.accent)
                .border_radius(6.0)
                .hover(move |style| {
                    style
                        .background(RSS_FORM_ACCENT_HOVER)
                        .border_color(RSS_FORM_ACCENT_HOVER)
                })
                .disabled(move |style| {
                    style
                        .cursor(CursorStyle::Default)
                        .background(palette.divider)
                        .border_color(palette.divider)
                        .color(palette.muted)
                })
        }),
    ))
    .style(|style| style.width_full().items_center().margin_top(8.0));
    v_stack((header, input, status, footer)).style(|style| style.width_full().gap(RSS_FORM_GAP_PX))
}

fn protection_popover(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    open: RwSignal<bool>,
    palette: Palette,
) -> impl IntoView {
    let lock_model = model.clone();
    let disable_model = model;
    menu(
        vec![
            MenuEntry::action(
                ICON_LOCK,
                || tr!(LockNote),
                || true,
                move || {
                    open.set(false);
                    lock_model.borrow_mut().lock_selected();
                    revision.update(|value| *value += 1);
                    schedule_autosave(lock_model.clone(), revision);
                },
            ),
            MenuEntry::action(
                ICON_UNLOCK,
                || tr!(RemoveEncryption),
                || true,
                move || {
                    open.set(false);
                    disable_model.borrow_mut().disable_protection_selected();
                    revision.update(|value| *value += 1);
                    schedule_autosave(disable_model.clone(), revision);
                },
            )
            .danger(true),
        ],
        palette,
    )
}

fn sidebar_sort_popover(
    scope: SidebarFilter,
    model: Rc<RefCell<AppModel>>,
    signals: CategorySortPopoverSignals,
    palette: Palette,
) -> impl IntoView {
    let CategorySortPopoverSignals {
        sidebar_state,
        revision,
        open,
        field,
        direction,
    } = signals;
    let apply_scope = scope;
    let apply_model = model;
    menu(
        vec![
            MenuEntry::action(
                ICON_SORT,
                || tr!(ByName),
                || true,
                move || field.set(NoteSortField::Name),
            )
            .selected(move || field.get() == NoteSortField::Name)
            .keep_open(),
            MenuEntry::action(
                ICON_SORT,
                || tr!(ByCreated),
                || true,
                move || field.set(NoteSortField::Created),
            )
            .selected(move || field.get() == NoteSortField::Created)
            .keep_open(),
            MenuEntry::action(
                ICON_SORT,
                || tr!(ByUpdated),
                || true,
                move || field.set(NoteSortField::Modified),
            )
            .selected(move || field.get() == NoteSortField::Modified)
            .keep_open(),
            MenuEntry::action(
                ICON_SORT,
                || tr!(Ascending),
                || true,
                move || direction.set(SortDirection::Ascending),
            )
            .selected(move || direction.get() == SortDirection::Ascending)
            .keep_open(),
            MenuEntry::action(
                ICON_SORT,
                || tr!(Descending),
                || true,
                move || direction.set(SortDirection::Descending),
            )
            .selected(move || direction.get() == SortDirection::Descending)
            .keep_open(),
            MenuEntry::action(
                ButtonAction::Save.icon(),
                || tr!(Apply),
                || true,
                move || {
                    let cleared = apply_model
                        .borrow_mut()
                        .clear_sidebar_note_order(&apply_scope);
                    if cleared.is_none() {
                        revision.update(|value| *value = value.saturating_add(1));
                        return;
                    }
                    if let Some(order_key) = sidebar_note_order_key(&apply_scope) {
                        sidebar_state.update(|state| {
                            state.set_note_sort(
                                order_key.to_owned(),
                                NoteSort {
                                    field: field.get_untracked(),
                                    direction: direction.get_untracked(),
                                },
                            );
                        });
                    }
                    open.set(false);
                    revision.update(|value| *value = value.saturating_add(1));
                },
            )
            .keep_open(),
        ],
        palette,
    )
}

fn sidebar_group_row(
    group: (SidebarFilter, String, usize, usize),
    model: Rc<RefCell<AppModel>>,
    sidebar_state: RwSignal<SidebarState>,
    category_drag: RwSignal<CategoryDragState>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let (filter, title, count, depth) = group;
    let row_hovered = create_rw_signal(false);
    let expanded_filter = filter.clone();
    let collapsed_filter = filter.clone();
    let category_path = match &filter {
        SidebarFilter::Tag(path) => Some(path.clone()),
        _ => None,
    };
    let sortable_scope = sidebar_note_order_key(&filter).map(|_| filter.clone());
    let sort_action = if let Some(scope) = sortable_scope.clone() {
        let order_key = sidebar_note_order_key(&scope)
            .expect("sortable sidebar group must have a canonical order key");
        let current_sort = sidebar_state.get_untracked().note_sort(order_key);
        let open = create_rw_signal(false);
        let field = create_rw_signal(current_sort.field);
        let direction = create_rw_signal(current_sort.direction);
        let trigger_scope = scope.clone();
        let trigger = sidebar_sort_button(row_hovered, palette, move || {
            let order_key = sidebar_note_order_key(&trigger_scope)
                .expect("sortable sidebar group must have a canonical order key");
            let current = sidebar_state.get_untracked().note_sort(order_key);
            field.set(current.field);
            direction.set(current.direction);
            open.set(!open.get_untracked());
        });
        let content_model = model.clone();
        anchored_popover(
            trigger,
            open,
            SORT_POPOVER_WIDTH_PX,
            4.0,
            false,
            move || {
                sidebar_sort_popover(
                    scope.clone(),
                    content_model.clone(),
                    CategorySortPopoverSignals {
                        sidebar_state,
                        revision,
                        open,
                        field,
                        direction,
                    },
                    palette,
                )
            },
        )
        .into_any()
    } else {
        empty().into_any()
    };
    let chevron = stack((
        svg(ICON_CHEVRON_DOWN).style(move |style| {
            let style = style.size(13.0, 13.0);
            if sidebar_state.get().is_expanded(&expanded_filter) {
                style
            } else {
                style.hide()
            }
        }),
        svg(ICON_CHEVRON_RIGHT)
            .update_value(move || {
                if i18n::current().is_rtl() {
                    ButtonAction::Back.icon()
                } else {
                    ICON_CHEVRON_RIGHT
                }
            })
            .style(move |style| {
                let style = style.size(13.0, 13.0);
                if sidebar_state.get().is_expanded(&collapsed_filter) {
                    style.hide()
                } else {
                    style
                }
            }),
    ))
    .style(move |style| {
        style
            .size(13.0, 13.0)
            .flex_shrink(0.0)
            .color(palette.sidebar_muted)
    });
    let style_path = category_path.clone();
    let title_filter = filter.clone();
    let row = h_stack((
        chevron,
        label(move || match &title_filter {
            SidebarFilter::All => tr!(All),
            SidebarFilter::Favorites => tr!(Favorites),
            SidebarFilter::Trash => tr!(Trash),
            SidebarFilter::Tag(_) => title.clone(),
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .color(palette.sidebar_ink)
                .min_width(0.0)
                .flex_shrink(1.0)
                .text_ellipsis()
                .selectable(false)
        }),
        empty().style(|style| style.flex_grow(1.0)),
        sort_action,
        text(count).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.sidebar_muted)
                .flex_shrink(0.0)
                .selectable(false)
        }),
    ))
    .style(|style| {
        rtl_row(style)
            .width_full()
            .min_width(0.0)
            .items_center()
            .gap(8.0)
    })
    .style(move |style| {
        let indent = sidebar_tree_indent(depth);
        let mut style = style
            .width_full()
            .height(SIDEBAR_GROUP_ROW_HEIGHT_PX)
            .items_center()
            .padding_left(8.0 + indent)
            .padding_right(8.0)
            .background(Color::TRANSPARENT)
            .color(palette.sidebar_ink)
            .border_radius(6.0)
            .hover(move |style| {
                style
                    .background(palette.sidebar_active)
                    .color(palette.sidebar_ink)
            });
        if let Some(path) = style_path.as_deref() {
            let drag = category_drag.get();
            if drag.source.as_deref() == Some(path) {
                style = style
                    .background(palette.sidebar_active)
                    .color(palette.sidebar_muted);
            }
            if let Some((target, position)) = drag.target.as_ref()
                && target == path
            {
                style = match position {
                    CategoryDropPosition::Before => {
                        style.border_top(2.0).border_color(palette.sidebar_accent)
                    }
                    CategoryDropPosition::After => style
                        .border_bottom(2.0)
                        .border_color(palette.sidebar_accent),
                };
            }
        }
        style
    })
    .on_event(EventListener::PointerMove, move |_| {
        if !row_hovered.get_untracked() {
            row_hovered.set(true);
        }
        EventPropagation::Continue
    })
    .on_event(EventListener::PointerLeave, move |_| {
        row_hovered.set(false);
        EventPropagation::Continue
    });

    if filter == SidebarFilter::Favorites {
        let pointer_filter = filter.clone();
        let pointer_model = model.clone();
        let keyboard_filter = filter;
        let keyboard_model = model;
        return SidebarGroupPointerView::new(row, move || {
            activate_sidebar_group(&pointer_filter, &pointer_model, sidebar_state, revision);
        })
        .keyboard_navigable()
        .on_event(EventListener::KeyDown, move |event| {
            if is_keyboard_activation(event) {
                activate_sidebar_group(&keyboard_filter, &keyboard_model, sidebar_state, revision);
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .into_any();
    }

    if category_path.is_none() {
        let action_filter = filter;
        return selectable_row(row, move || {
            activate_sidebar_group(&action_filter, &model, sidebar_state, revision);
        })
        .into_any();
    };

    let keyboard_filter = filter;
    let keyboard_model = model;

    row.keyboard_navigable()
        .on_event(EventListener::KeyDown, move |event| {
            if is_keyboard_activation(event) {
                activate_sidebar_group(&keyboard_filter, &keyboard_model, sidebar_state, revision);
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .into_any()
}

fn external_group_row(count: usize, palette: Palette) -> AnyView {
    h_stack((
        svg(ICON_CHEVRON_DOWN).style(move |style| {
            style
                .size(13.0, 13.0)
                .flex_shrink(0.0)
                .color(palette.sidebar_muted)
        }),
        label(move || tr!(External)).style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .color(palette.sidebar_ink)
                .selectable(false)
        }),
        empty().style(|style| style.flex_grow(1.0)),
        text(count).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.sidebar_muted)
                .selectable(false)
        }),
    ))
    .style(move |style| {
        rtl_row(style)
            .width_full()
            .height(SIDEBAR_GROUP_ROW_HEIGHT_PX)
            .items_center()
            .gap(8.0)
            .padding_horiz(8.0)
            .color(palette.sidebar_ink)
    })
    .into_any()
}

fn external_file_row(
    file: ExternalFileSummary,
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let hovered = create_rw_signal(false);
    let target = DocumentTarget::ExternalFile {
        engine_id: file.engine_id.clone(),
        item_id: file.item_id.clone(),
    };
    let open_model = model.clone();
    let open_path = file.path.clone();
    let close_model = model.clone();
    let close_target = target.clone();
    let selected_model = model;
    let selected_target = target;
    let is_ready = matches!(file.availability, stillus_core::ItemAvailability::Ready);
    let tooltip: UiText = match &file.availability {
        stillus_core::ItemAvailability::Ready => file.path.display().to_string().into(),
        stillus_core::ItemAvailability::NeedsUnlock => {
            msg!(FileLocked , "value" => file.path.display().to_string()).into()
        }
        stillus_core::ItemAvailability::Invalid(message)
        | stillus_core::ItemAvailability::Unavailable(message) => format!(
            "{}\n{}",
            file.path.display(),
            i18n::user_error_text(&UiText::from(message.as_str()))
        )
        .into(),
    };
    let main = anchored_tooltip(
        selectable_row(
            h_stack((
                svg(ICON_NOTE).style(|style| style.size(13.0, 13.0).flex_shrink(0.0)),
                text(file.title).style(move |style| {
                    style
                        .font_size(crate::ui::FONT_BODY as f32)
                        .color(if is_ready {
                            palette.sidebar_ink
                        } else {
                            Color::rgb8(224, 160, 140)
                        })
                        .min_width(0.0)
                        .flex_shrink(1.0)
                        .text_ellipsis()
                        .selectable(false)
                }),
            ))
            .style(|style| {
                rtl_row(style)
                    .min_width(0.0)
                    .items_center()
                    .gap(7.0)
                    .flex_grow(1.0)
            }),
            move || {
                open_model.borrow_mut().open_external_path(&open_path);
                revision.update(|value| *value = value.saturating_add(1));
                schedule_autosave(open_model.clone(), revision);
            },
        ),
        Rc::new(move || tooltip.to_string()),
        palette,
    )
    .style(|style| style.min_width(0.0).flex_grow(1.0).height_full());
    let close = compact_icon_button(
        || ButtonAction::Close.icon(),
        || tr!(RemoveSidebar),
        IconButtonTone::Sidebar,
        palette,
        22.0,
        || true,
        move || {
            close_model
                .borrow_mut()
                .close_external_target(close_target.clone());
            revision.update(|value| *value = value.saturating_add(1));
            schedule_autosave(close_model.clone(), revision);
        },
    )
    .style(move |style| {
        style
            .size(22.0, 22.0)
            .items_center()
            .justify_center()
            .flex_shrink(0.0)
            .border_radius(4.0)
            .color(if hovered.get() {
                palette.sidebar_muted
            } else {
                Color::TRANSPARENT
            })
            .hover(move |style| {
                style
                    .color(palette.sidebar_ink)
                    .background(palette.sidebar_active)
            })
            .focus_visible(move |style| style.color(palette.sidebar_ink))
    });
    h_stack((main, close))
        .style(move |style| {
            revision.get();
            let selected = selected_model
                .borrow()
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_target)
                .as_ref()
                == Some(&selected_target);
            rtl_row(style)
                .width_full()
                .height(SIDEBAR_NOTE_ROW_HEIGHT_PX)
                .items_center()
                .padding_left(if i18n::current().is_rtl() { 5.0 } else { 30.0 })
                .padding_right(if i18n::current().is_rtl() { 30.0 } else { 5.0 })
                .gap(4.0)
                .background(if selected {
                    palette.accent
                } else {
                    Color::TRANSPARENT
                })
                .border_radius(6.0)
                .hover(move |style| {
                    style.background(if selected {
                        palette.accent
                    } else {
                        palette.sidebar_active
                    })
                })
        })
        .on_event(EventListener::PointerMove, move |_| {
            hovered.set(true);
            EventPropagation::Continue
        })
        .on_event(EventListener::PointerLeave, move |_| {
            hovered.set(false);
            EventPropagation::Continue
        })
        .into_any()
}

fn sidebar_note_row(
    parent: SidebarFilter,
    depth: usize,
    note: stillus_core::NoteSummary,
    model: Rc<RefCell<AppModel>>,
    signals: SidebarNoteSignals,
    palette: Palette,
) -> AnyView {
    let SidebarNoteSignals {
        sidebar_state,
        note_drag,
        revision,
    } = signals;
    let row_model = model.clone();
    let selected_model = model.clone();
    let action_path = note.path.clone();
    let action_parent = parent.clone();
    let selected_path = note.path.clone();
    let full_title = note.title.clone();
    let is_ready = note.availability.is_ready();
    let protected = note.protection == NoteProtection::Protected;
    let pinned = note.pinned;
    let favorited = note.favorited;
    let status_icons = dyn_stack(
        move || sidebar_note_indicator_icons(protected, pinned, favorited),
        |icon| *icon,
        move |icon| svg(icon).style(|style| style.size(12.0, 12.0).flex_shrink(0.0)),
    )
    .style(|style| style.items_center().gap(4.0).flex_shrink(0.0));
    let content = h_stack((
        svg(ICON_NOTE).style(|style| style.size(13.0, 13.0).flex_shrink(0.0)),
        // Navigation labels never own text selection: a selectable label
        // keeps a pending selection when a modal steals its pointer-up and
        // then captures the next click anywhere in the window.
        ui::anchored_tooltip(
            text(note_caption(&note)),
            Rc::new(move || full_title.clone()),
            palette,
        )
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .color(if is_ready {
                    palette.sidebar_ink
                } else {
                    Color::rgb8(224, 160, 140)
                })
                .min_width(0.0)
                .flex_shrink(1.0)
                .text_ellipsis()
                .selectable(false)
        }),
        empty().style(|style| style.flex_grow(1.0)),
        status_icons,
    ))
    .style(|style| style.width_full().min_width(0.0).items_center().gap(7.0));
    let activate: Rc<dyn Fn()> = Rc::new(move || {
        sidebar_state.update(|state| state.use_group(action_parent.clone()));
        let index = row_model.borrow().workspace.as_ref().and_then(|workspace| {
            workspace
                .notes()
                .iter()
                .position(|candidate| candidate.path == action_path)
        });
        if let Some(index) = index {
            row_model.borrow_mut().open_note(index);
            revision.update(|value| *value += 1);
            schedule_autosave(row_model.clone(), revision);
        }
    });
    let style_item = CatalogOrderItem::Note(note.path.clone());
    let row_style = move |style: Style| {
        revision.get();
        let selected = selected_model
            .borrow()
            .workspace
            .as_ref()
            .and_then(|workspace| {
                workspace
                    .selected_note()
                    .and_then(|index| workspace.notes().get(index))
            })
            .is_some_and(|candidate| candidate.path == selected_path);
        let (background, hover_background, foreground) = if selected {
            (palette.accent, palette.accent, palette.sidebar_ink)
        } else {
            (
                Color::TRANSPARENT,
                palette.sidebar_active,
                palette.sidebar_muted,
            )
        };
        let mut style = rtl_row(style)
            .width_full()
            .height(SIDEBAR_NOTE_ROW_HEIGHT_PX)
            .items_center()
            .padding_left(if i18n::current().is_rtl() {
                8.0
            } else {
                30.0 + sidebar_tree_indent(depth)
            })
            .padding_right(if i18n::current().is_rtl() {
                30.0 + sidebar_tree_indent(depth)
            } else {
                8.0
            })
            .background(background)
            .color(foreground)
            .border_radius(6.0)
            .hover(move |style| style.background(hover_background).color(foreground));
        let drag = note_drag.get();
        if drag.source.as_ref() == Some(&style_item) {
            style = style
                .background(palette.sidebar_active)
                .color(palette.sidebar_muted);
        }
        if let Some((target, position)) = drag.target.as_ref()
            && target == &style_item
        {
            style = match position {
                CategoryDropPosition::Before => {
                    style.border_top(2.0).border_color(palette.sidebar_accent)
                }
                CategoryDropPosition::After => style
                    .border_bottom(2.0)
                    .border_color(palette.sidebar_accent),
            };
        }
        style
    };

    let content = content.style(row_style);
    if sidebar_note_order_key(&parent).is_none() {
        let click = activate.clone();
        return selectable_row(content, move || click()).into_any();
    }
    let drag_item = CatalogOrderItem::Note(note.path.clone());
    let drag_group = parent.clone();
    let drag_model = model.clone();
    let drop_item = CatalogOrderItem::Note(note.path.clone());
    let drop_group = parent;
    let drop_model = model;
    let click = activate.clone();
    let keyboard_click = activate;
    let view = NotePointerDragView::new(
        content,
        move || click(),
        move |delta_y| {
            let siblings = ordered_sidebar_catalog_items(
                &drag_model.borrow(),
                &sidebar_state.get_untracked(),
                &drag_group,
            );
            note_drag.set(NoteDragState {
                source: Some(drag_item.clone()),
                target: note_drop_target(&siblings, &drag_item, pinned, delta_y),
            });
        },
        move || {
            let target = note_drag.get_untracked().target;
            note_drag.set(NoteDragState::default());
            let Some((target, position)) = target else {
                return;
            };
            let siblings = ordered_sidebar_catalog_items(
                &drop_model.borrow(),
                &sidebar_state.get_untracked(),
                &drop_group,
            );
            let Some(items) = reordered_catalog_items(&siblings, &drop_item, &target, position)
            else {
                return;
            };
            let changed = drop_model
                .borrow_mut()
                .set_sidebar_catalog_order(&drop_group, &items);
            if changed == Some(true) {
                if let Some(order_key) = sidebar_note_order_key(&drop_group) {
                    sidebar_state.update(|state| state.use_manual_note_order(order_key));
                }
            }
            revision.update(|value| *value = value.saturating_add(1));
        },
        move || note_drag.set(NoteDragState::default()),
    )
    .keyboard_navigable()
    .on_event(EventListener::KeyDown, move |event| {
        if is_keyboard_activation(event) {
            keyboard_click();
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    });
    view.into_any()
}

fn engine_sidebar_row(
    parent: SidebarFilter,
    depth: usize,
    summary: stillus_engine::ItemSummary,
    model: Rc<RefCell<AppModel>>,
    signals: SidebarNoteSignals,
    palette: Palette,
) -> AnyView {
    let SidebarNoteSignals {
        sidebar_state,
        note_drag,
        revision,
    } = signals;
    let engine = summary.engine_id.clone();
    let item_id = summary.item_id.clone();
    let selected_engine = engine.clone();
    let activate_engine = engine.clone();
    let drag_engine = engine.clone();
    let selected_id = item_id.clone();
    let selected_model = model.clone();
    let activate_model = model.clone();
    let activate_id = item_id.clone();
    let activate_parent = parent.clone();
    let title = summary.metadata.title;
    let full_title = title.clone();
    let unread = summary.badge.unwrap_or(0);
    let badge_model = model.clone();
    let badge_id = item_id.clone();
    let badge_style_model = model.clone();
    let badge_style_id = item_id.clone();
    let chat_badge = engine == stillus_chat::engine_id();
    let pinned = summary.metadata.pinned;
    let ready = matches!(summary.availability, stillus_core::ItemAvailability::Ready);
    let content = h_stack((
        svg(if engine == stillus_chat::engine_id() {
            ICON_CHAT
        } else {
            ICON_RSS
        })
        .style(move |style| {
            style.size(13.0, 13.0).flex_shrink(0.0).color(if ready {
                palette.sidebar_muted
            } else {
                Color::rgb8(224, 160, 140)
            })
        }),
        ui::anchored_tooltip(text(title), Rc::new(move || full_title.clone()), palette).style(
            move |style| {
                style
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.sidebar_ink)
                    .min_width(0.0)
                    .flex_shrink(1.0)
                    .text_ellipsis()
                    .selectable(false)
            },
        ),
        empty().style(|style| style.flex_grow(1.0)),
        label(move || {
            revision.get();
            if chat_badge && badge_model.borrow().chat_running(&badge_id) {
                "…".to_owned()
            } else if chat_badge && unread == 0 {
                String::new()
            } else {
                unread.to_string()
            }
        })
        .style(move |style| {
            style
                .min_width(20.0)
                .padding_horiz(5.0)
                .height(18.0)
                .items_center()
                .justify_center()
                .border_radius(9.0)
                .font_size(crate::ui::FONT_CAPTION as f32)
                .font_weight(floem::text::Weight::SEMIBOLD)
                .background(
                    if chat_badge
                        && unread == 0
                        && !{
                            revision.get();
                            badge_style_model.borrow().chat_running(&badge_style_id)
                        }
                    {
                        Color::TRANSPARENT
                    } else if unread > 0 {
                        palette.sidebar_accent
                    } else {
                        palette.sidebar_active
                    },
                )
                .color(palette.sidebar_ink)
                .selectable(false)
        }),
    ))
    .style({
        let style_item = CatalogOrderItem::Engine(engine.clone(), item_id.clone());
        move |style| {
            revision.get();
            let selected = selected_model
                .borrow()
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_engine_item)
                == Some(&(selected_engine.clone(), selected_id.clone()));
            let mut style = rtl_row(style)
                .width_full()
                .height(SIDEBAR_NOTE_ROW_HEIGHT_PX)
                .min_width(0.0)
                .items_center()
                .gap(7.0)
                .padding_left(30.0 + sidebar_tree_indent(depth))
                .padding_right(8.0)
                .border_radius(6.0)
                .background(if selected {
                    palette.accent
                } else {
                    Color::TRANSPARENT
                })
                .hover(move |style| {
                    style.background(if selected {
                        palette.accent
                    } else {
                        palette.sidebar_active
                    })
                });
            let drag = note_drag.get();
            if drag.source.as_ref() == Some(&style_item) {
                style = style
                    .background(palette.sidebar_active)
                    .color(palette.sidebar_muted);
            }
            if let Some((target, position)) = drag.target.as_ref()
                && target == &style_item
            {
                style = match position {
                    CategoryDropPosition::Before => {
                        style.border_top(2.0).border_color(palette.sidebar_accent)
                    }
                    CategoryDropPosition::After => style
                        .border_bottom(2.0)
                        .border_color(palette.sidebar_accent),
                };
            }
            style
        }
    });
    let activate: Rc<dyn Fn()> = Rc::new(move || {
        sidebar_state.update(|state| state.use_group(activate_parent.clone()));
        if activate_engine == stillus_core::rss_engine_id() {
            let opened = activate_model.borrow_mut().open_rss(&activate_id);
            if opened
                && activate_model
                    .borrow_mut()
                    .start_rss_refresh(activate_id.clone())
            {
                schedule_rss_poll(activate_model.clone(), revision);
            }
        } else {
            let _ = activate_model.borrow_mut().dispatch(
                application::api::Caller::Ui,
                application::api::Command::Chat(application::chat::Command::Open {
                    id: activate_id.to_string(),
                }),
            );
        }
        revision.update(|value| *value = value.saturating_add(1));
        schedule_autosave(activate_model.clone(), revision);
    });
    if sidebar_note_order_key(&parent).is_none() {
        let click = activate;
        return selectable_row(content, move || click()).into_any();
    }
    let drag_item = CatalogOrderItem::Engine(drag_engine.clone(), item_id.clone());
    let drag_group = parent.clone();
    let drag_model = model.clone();
    let drop_item = CatalogOrderItem::Engine(drag_engine, item_id);
    let drop_group = parent;
    let drop_model = model;
    let click = activate.clone();
    let keyboard_click = activate;
    NotePointerDragView::new(
        content,
        move || click(),
        move |delta_y| {
            let siblings = ordered_sidebar_catalog_items(
                &drag_model.borrow(),
                &sidebar_state.get_untracked(),
                &drag_group,
            );
            note_drag.set(NoteDragState {
                source: Some(drag_item.clone()),
                target: note_drop_target(&siblings, &drag_item, pinned, delta_y),
            });
        },
        move || {
            let target = note_drag.get_untracked().target;
            note_drag.set(NoteDragState::default());
            let Some((target, position)) = target else {
                return;
            };
            let siblings = ordered_sidebar_catalog_items(
                &drop_model.borrow(),
                &sidebar_state.get_untracked(),
                &drop_group,
            );
            let Some(items) = reordered_catalog_items(&siblings, &drop_item, &target, position)
            else {
                return;
            };
            let changed = drop_model
                .borrow_mut()
                .set_sidebar_catalog_order(&drop_group, &items);
            if changed == Some(true)
                && let Some(order_key) = sidebar_note_order_key(&drop_group)
            {
                sidebar_state.update(|state| state.use_manual_note_order(order_key));
            }
            revision.update(|value| *value = value.saturating_add(1));
        },
        move || note_drag.set(NoteDragState::default()),
    )
    .keyboard_navigable()
    .on_event(EventListener::KeyDown, move |event| {
        if is_keyboard_activation(event) {
            keyboard_click();
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    })
    .into_any()
}

#[derive(Clone, Debug, PartialEq)]
struct SettingsFeedback {
    message: UiText,
    is_error: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsSection {
    General,
    Encryption,
    Ai,
    Updates,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncryptionField {
    Current,
    New,
    Confirmation,
}

#[derive(Clone, Copy)]
struct EncryptionFieldIds {
    current: ViewId,
    new_password: ViewId,
    confirmation: ViewId,
}

impl EncryptionFieldIds {
    fn get(self, field: EncryptionField) -> ViewId {
        match field {
            EncryptionField::Current => self.current,
            EncryptionField::New => self.new_password,
            EncryptionField::Confirmation => self.confirmation,
        }
    }

    fn adjacent(self, field: EncryptionField, reverse: bool) -> EncryptionField {
        match (field, reverse) {
            (EncryptionField::Current, false) | (EncryptionField::Confirmation, true) => {
                EncryptionField::New
            }
            (EncryptionField::New, false) => EncryptionField::Confirmation,
            (EncryptionField::New, true) => EncryptionField::Current,
            (EncryptionField::Confirmation, false) => EncryptionField::Current,
            (EncryptionField::Current, true) => EncryptionField::Confirmation,
        }
    }
}

struct EncryptionEntry {
    current: Zeroizing<String>,
    new_password: Zeroizing<String>,
    confirmation: Zeroizing<String>,
    active: EncryptionField,
}

impl Default for EncryptionEntry {
    fn default() -> Self {
        Self {
            current: Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES)),
            new_password: Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES)),
            confirmation: Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES)),
            active: EncryptionField::Current,
        }
    }
}

impl EncryptionEntry {
    fn field(&self, field: EncryptionField) -> &str {
        match field {
            EncryptionField::Current => &self.current,
            EncryptionField::New => &self.new_password,
            EncryptionField::Confirmation => &self.confirmation,
        }
    }

    fn field_mut(&mut self, field: EncryptionField) -> &mut String {
        match field {
            EncryptionField::Current => &mut self.current,
            EncryptionField::New => &mut self.new_password,
            EncryptionField::Confirmation => &mut self.confirmation,
        }
    }

    fn clear(&mut self) {
        self.current.zeroize();
        self.new_password.zeroize();
        self.confirmation.zeroize();
        self.active = EncryptionField::Current;
    }

    fn clear_current(&mut self) {
        self.current.zeroize();
        self.active = EncryptionField::Current;
    }

    fn all_fields_empty(&self) -> bool {
        self.current.is_empty() && self.new_password.is_empty() && self.confirmation.is_empty()
    }
}

#[derive(Clone, Copy)]
struct SettingsPageSignals {
    open: RwSignal<bool>,
    section: RwSignal<SettingsSection>,
    path: RwSignal<String>,
    feedback: RwSignal<Option<SettingsFeedback>>,
    picker_active: RwSignal<bool>,
    encryption_entry: RwSignal<EncryptionEntry>,
    encryption_revision: RwSignal<u64>,
    encryption_feedback: RwSignal<Option<SettingsFeedback>>,
}

#[derive(Clone, Copy)]
struct SearchPanelSignals {
    open: RwSignal<bool>,
    query: RwSignal<String>,
    selected: RwSignal<usize>,
    editor_focus_request: RwSignal<u64>,
}

fn sidebar_resize_handle(
    sidebar_width: RwSignal<f64>,
    actual_width: floem::reactive::Memo<f64>,
    palette: Palette,
) -> impl IntoView {
    let hovered = create_rw_signal(false);
    let dragging = create_rw_signal(false);
    let grab_x = create_rw_signal(None::<f64>);
    let hit_surface =
        empty().style(move |style| style.width_full().height_full().background(palette.sidebar));
    SidebarResizeView::new(
        stack((hit_surface,)),
        sidebar_width,
        actual_width,
        hovered,
        dragging,
        grab_x,
    )
    .style(|style| {
        style
            .absolute()
            .inset_right(if i18n::current().is_rtl() {
                floem::unit::PxPctAuto::Auto
            } else {
                floem::unit::PxPctAuto::Px(0.0)
            })
            .inset_left(if i18n::current().is_rtl() {
                floem::unit::PxPctAuto::Px(0.0)
            } else {
                floem::unit::PxPctAuto::Auto
            })
            .width(8.0)
            .height_full()
            .cursor(CursorStyle::ColResize)
            .z_index(10)
    })
}

struct SidebarResizeView {
    id: ViewId,
    actual_width: floem::reactive::Memo<f64>,
    sidebar_width: RwSignal<f64>,
    hovered: RwSignal<bool>,
    dragging: RwSignal<bool>,
    grab_x: RwSignal<Option<f64>>,
}

impl SidebarResizeView {
    fn new(
        child: impl IntoView,
        sidebar_width: RwSignal<f64>,
        actual_width: floem::reactive::Memo<f64>,
        hovered: RwSignal<bool>,
        dragging: RwSignal<bool>,
        grab_x: RwSignal<Option<f64>>,
    ) -> Self {
        let id = ViewId::new();
        id.add_child(Box::new(child.into_view()));
        Self {
            id,
            sidebar_width,
            actual_width,
            hovered,
            dragging,
            grab_x,
        }
    }
}

impl View for SidebarResizeView {
    fn id(&self) -> ViewId {
        self.id
    }

    fn event_before_children(
        &mut self,
        _cx: &mut floem::context::EventCx,
        event: &Event,
    ) -> EventPropagation {
        match event {
            Event::PointerMove(pointer) => {
                if let Some(grab_x) = self.grab_x.get_untracked() {
                    let width = resized_sidebar_width(
                        self.actual_width.get_untracked(),
                        if i18n::current().is_rtl() {
                            2.0 * grab_x - pointer.pos.x
                        } else {
                            pointer.pos.x
                        },
                        grab_x,
                    );
                    self.sidebar_width.set(width);
                    self.hovered.set(true);
                    EventPropagation::Stop
                } else {
                    self.hovered.set(true);
                    EventPropagation::Continue
                }
            }
            Event::PointerLeave => {
                if !self.dragging.get_untracked() {
                    self.hovered.set(false);
                }
                EventPropagation::Continue
            }
            Event::PointerDown(pointer) => {
                if !pointer.button.is_primary() {
                    return EventPropagation::Continue;
                }
                self.grab_x.set(Some(pointer.pos.x));
                self.dragging.set(true);
                self.hovered.set(true);
                self.id.request_active();
                EventPropagation::Stop
            }
            Event::PointerUp(pointer) => {
                if !pointer.button.is_primary() || !self.dragging.get_untracked() {
                    return EventPropagation::Continue;
                }
                self.dragging.set(false);
                self.hovered.set(false);
                self.grab_x.set(None);
                EventPropagation::Stop
            }
            _ => EventPropagation::Continue,
        }
    }
}

struct SidebarLayoutSignals {
    sidebar_width: RwSignal<f64>,
    sidebar_state: RwSignal<SidebarState>,
    window_size: RwSignal<Size>,
}

fn sidebar_panel(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    layout: SidebarLayoutSignals,
    search: SearchPanelSignals,
    open_settings: Rc<dyn Fn()>,
    palette: Palette,
) -> impl IntoView {
    let SidebarLayoutSignals {
        sidebar_width,
        sidebar_state,
        window_size,
    } = layout;
    let SearchPanelSignals {
        open: search_open,
        query: search_query,
        selected: search_selected,
        editor_focus_request,
    } = search;
    let reconcile_model = model.clone();
    create_effect(move |_| {
        revision.get();
        let categories = reconcile_model
            .borrow()
            .workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .categories()
                    .iter()
                    .map(|category| category.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let current = sidebar_state.get_untracked();
        let mut reconciled = current.clone();
        reconciled.reconcile_categories(categories.iter().map(String::as_str));
        if reconciled != current {
            sidebar_state.set(reconciled);
        }
    });
    let actual_width = floem::reactive::create_memo(move |_| {
        displayed_sidebar_width(
            sidebar_width.get(),
            window_size.get().width,
            sidebar_state.get().collapsed,
        )
    });
    create_effect(move |_| {
        if search_open.get() && sidebar_state.get_untracked().collapsed {
            sidebar_state.update(|state| state.collapsed = false);
        }
    });
    let tree_state_model = model.clone();
    let tree_view_model = model.clone();
    let tree_pointer_model = model.clone();
    let tree_click_model = model.clone();
    let category_drag = create_rw_signal(CategoryDragState::default());
    let note_drag = create_rw_signal(NoteDragState::default());
    let category_pointer_drag = Rc::new(RefCell::new(CategoryPointerDrag::default()));
    let tree_rows = dyn_stack(
        move || {
            revision.get();
            let state = sidebar_state.get();
            let model = tree_state_model.borrow();
            let Some(workspace) = model.workspace.as_ref() else {
                return Vec::new();
            };
            let notes = workspace.notes();
            let rss = workspace.non_document_items();
            let external_files = workspace.external_files();
            current_sidebar_rows(&model, &state)
                .into_iter()
                .map(|row| match row {
                    SidebarRow::ExternalGroup { count } => SidebarItem::ExternalGroup { count },
                    SidebarRow::ExternalFile { index } => {
                        SidebarItem::ExternalFile(external_files[index].clone())
                    }
                    SidebarRow::Group {
                        filter,
                        title,
                        count,
                        depth,
                    } => SidebarItem::Group {
                        filter,
                        title,
                        count,
                        depth,
                    },
                    SidebarRow::Note {
                        parent,
                        index,
                        depth,
                    } => SidebarItem::Note {
                        parent,
                        depth,
                        note: notes[index].clone(),
                    },
                    SidebarRow::Engine {
                        parent,
                        index,
                        depth,
                    } => SidebarItem::Engine {
                        parent,
                        depth,
                        summary: rss[index].clone(),
                    },
                    SidebarRow::Separator => SidebarItem::Separator,
                })
                .collect::<Vec<_>>()
        },
        SidebarItem::key,
        move |item| match item {
            SidebarItem::ExternalGroup { count } => external_group_row(count, palette),
            SidebarItem::ExternalFile(file) => {
                external_file_row(file, tree_view_model.clone(), revision, palette)
            }
            SidebarItem::Group {
                filter,
                title,
                count,
                depth,
            } => sidebar_group_row(
                (filter, title, count, depth),
                tree_view_model.clone(),
                sidebar_state,
                category_drag,
                revision,
                palette,
            )
            .into_any(),
            SidebarItem::Note {
                parent,
                depth,
                note,
            } => sidebar_note_row(
                parent,
                depth,
                note,
                tree_view_model.clone(),
                SidebarNoteSignals {
                    sidebar_state,
                    note_drag,
                    revision,
                },
                palette,
            )
            .into_any(),
            SidebarItem::Engine {
                parent,
                depth,
                summary,
            } => engine_sidebar_row(
                parent,
                depth,
                summary,
                tree_view_model.clone(),
                SidebarNoteSignals {
                    sidebar_state,
                    note_drag,
                    revision,
                },
                palette,
            ),
            SidebarItem::Separator => empty()
                .style(|style| style.width_full().height(SIDEBAR_SECTION_GAP_PX))
                .into_any(),
        },
    )
    .style(|style| style.flex_col().width_full().gap(SIDEBAR_TREE_ROW_GAP_PX));
    let tree_rows_id = tree_rows.id();
    let pointer_down_drag = category_pointer_drag.clone();
    let pointer_down_id = tree_rows_id;
    let pointer_move_drag = category_pointer_drag.clone();
    let pointer_move_id = tree_rows_id;
    let pointer_up_drag = category_pointer_drag;
    let tree_rows = tree_rows
        .on_event(EventListener::PointerDown, move |event| {
            let Event::PointerDown(pointer) = event else {
                return EventPropagation::Continue;
            };
            if !pointer.button.is_primary() {
                return EventPropagation::Continue;
            }
            let rows =
                current_sidebar_rows(&tree_pointer_model.borrow(), &sidebar_state.get_untracked());
            let hit_regions = category_drag_hit_regions(&rows);
            let width = pointer_down_id.get_size().map_or(0.0, |size| size.width);
            let Some(source) = category_drag_source_at_point(&hit_regions, pointer.pos, width)
            else {
                return EventPropagation::Continue;
            };
            *pointer_down_drag.borrow_mut() = CategoryPointerDrag {
                source: Some(source),
                origin: Some(pointer.pos),
                active: false,
                hit_regions,
            };
            category_drag.set(CategoryDragState::default());
            pointer_down_id.request_active();
            EventPropagation::Stop
        })
        .on_event(EventListener::PointerMove, move |event| {
            let Event::PointerMove(pointer) = event else {
                return EventPropagation::Continue;
            };
            let mut drag = pointer_move_drag.borrow_mut();
            let (Some(source), Some(origin)) = (drag.source.clone(), drag.origin) else {
                return EventPropagation::Continue;
            };
            if !drag.active && category_drag_threshold_reached(origin, pointer.pos) {
                drag.active = true;
            }
            if !drag.active {
                return EventPropagation::Stop;
            }
            let width = pointer_move_id.get_size().map_or(0.0, |size| size.width);
            let target =
                category_drop_target_at_point(&drag.hit_regions, &source, pointer.pos, width);
            let visual = CategoryDragState {
                source: Some(source),
                target,
            };
            drop(drag);
            if category_drag.get_untracked() != visual {
                category_drag.set(visual);
            }
            EventPropagation::Stop
        })
        .on_event(EventListener::PointerUp, move |event| {
            let Event::PointerUp(pointer) = event else {
                return EventPropagation::Continue;
            };
            if !pointer.button.is_primary() {
                return EventPropagation::Continue;
            }
            let mut pointer_drag = pointer_up_drag.borrow_mut();
            let Some(source) = pointer_drag.source.take() else {
                return EventPropagation::Continue;
            };
            let active = pointer_drag.active;
            *pointer_drag = CategoryPointerDrag::default();
            drop(pointer_drag);

            let target = category_drag.get_untracked().target;
            category_drag.set(CategoryDragState::default());
            if !active {
                activate_sidebar_group(
                    &SidebarFilter::Tag(source),
                    &tree_click_model,
                    sidebar_state,
                    revision,
                );
                return EventPropagation::Stop;
            }

            let Some((target, position)) = target else {
                return EventPropagation::Stop;
            };
            let mut changed = false;
            sidebar_state.update(|state| {
                changed = state.reorder_category(&source, &target, position);
            });
            if changed {
                revision.update(|value| *value += 1);
            }
            EventPropagation::Stop
        });
    let tree_scrollbar_visible = create_rw_signal(false);
    let tree_scrollbar_generation = create_rw_signal(0_u64);
    let tree_scroll_origin = create_rw_signal(None::<Point>);
    let tree = scroll(tree_rows)
        .on_scroll(move |viewport| {
            let origin = viewport.origin();
            let previous = tree_scroll_origin.get_untracked();
            tree_scroll_origin.set(Some(origin));
            if previous.is_some_and(|previous| previous != origin) {
                show_scrollbar_temporarily(tree_scrollbar_visible, tree_scrollbar_generation);
            }
        })
        .style(move |style| {
            let style = style.width_full().min_height(0.0).flex_grow(1.0);
            if search_open.get() || sidebar_state.get().collapsed {
                style.hide()
            } else {
                style
            }
        })
        .scroll_style(move |style| {
            search_open.get();
            style.hide_bars(!tree_scrollbar_visible.get())
        });

    let create_menu_open = create_rw_signal(false);
    let external_picker_active = create_rw_signal(false);
    let external_file_spec = model
        .borrow()
        .workspace
        .as_ref()
        .and_then(|workspace| external_file_picker_spec(workspace.external_file_extensions()));
    let create_trigger = icon_button(
        ButtonAction::Add.icon(),
        || tr!(CreateOrOpen),
        IconButtonTone::Primary,
        palette,
        move || create_menu_open.set(!create_menu_open.get_untracked()),
    );
    let create_popover_model = model.clone();
    let create_action = anchored_popover(
        create_trigger,
        create_menu_open,
        CREATE_POPOVER_WIDTH_PX,
        4.0,
        false,
        move || {
            creation_popover(
                create_popover_model.clone(),
                revision,
                sidebar_state,
                create_menu_open,
                external_picker_active,
                external_file_spec,
                palette,
            )
        },
    );
    let header = h_stack((
        icon_button(
            ButtonAction::Search.icon(),
            || tr!(SearchShortcut, "modifier" => i18n::shortcut_modifier()),
            IconButtonTone::Sidebar,
            palette,
            move || {
                search_open.set(true);
            },
        ),
        empty().style(|style| style.flex_grow(1.0)),
        create_action,
        icon_button(
            ButtonAction::Settings.icon(),
            || tr!(Settings),
            IconButtonTone::Sidebar,
            palette,
            move || open_settings(),
        ),
    ))
    .style(move |style| {
        rtl_row(style)
            .width_full()
            .height(if sidebar_state.get().collapsed {
                120.0
            } else {
                32.0
            })
            .flex_shrink(0.0)
            .items_center()
            .gap(6.0)
            .apply_if(sidebar_state.get().collapsed, |style| style.flex_col())
    });
    let rail = v_stack_from_iter(
        [
            (SidebarFilter::All, ICON_NOTE, i18n::Key::All),
            (
                SidebarFilter::Favorites,
                ButtonAction::Favorite.icon(),
                i18n::Key::Favorites,
            ),
            (
                SidebarFilter::Trash,
                ButtonAction::Delete.icon(),
                i18n::Key::Trash,
            ),
        ]
        .into_iter()
        .map(|(filter, icon, title)| {
            let model = model.clone();
            icon_button(
                icon,
                move || title.to_string(),
                IconButtonTone::Sidebar,
                palette,
                move || {
                    sidebar_state.update(|state| {
                        state.collapsed = false;
                        state.expanded.remove(&filter);
                    });
                    activate_sidebar_group(&filter, &model, sidebar_state, revision);
                },
            )
            .into_any()
        }),
    )
    .style(move |style| {
        style
            .width_full()
            .items_center()
            .gap(8.0)
            .apply_if(!sidebar_state.get().collapsed, |style| style.hide())
    });
    let collapse = dyn_container(
        move || sidebar_state.get().collapsed,
        move |collapsed| {
            icon_button(
                if collapsed {
                    ICON_CHEVRON_RIGHT
                } else {
                    ICON_BACK
                },
                move || {
                    if collapsed {
                        tr!(SidebarExpand)
                    } else {
                        tr!(SidebarCollapse)
                    }
                },
                IconButtonTone::Sidebar,
                palette,
                move || {
                    popover_close_all();
                    search_open.set(false);
                    sidebar_state.update(|state| state.collapsed = !state.collapsed);
                },
            )
            .into_any()
        },
    )
    .style(|style| style.height(32.0).flex_shrink(0.0));

    let search_rows_state_model = model.clone();
    let search_rows_view_model = model.clone();
    let search_rows = dyn_stack(
        move || {
            revision.get();
            let query = search_query.get();
            let model = search_rows_state_model.borrow();
            let Some(generation) = model.search_result_generation(&query) else {
                return Vec::new();
            };
            model
                .search_results
                .iter()
                .cloned()
                .enumerate()
                .map(|(row_index, result)| (generation, row_index, result))
                .collect::<Vec<_>>()
        },
        |(generation, _, result)| (*generation, result.relative_path.clone()),
        move |(generation, row_index, result)| {
            let row_model = search_rows_view_model.clone();
            let relative_path = result.relative_path.clone();
            let kind = match result.match_kind {
                MatchKind::Title => msg!(Title),
                MatchKind::Tag => msg!(Tag),
                MatchKind::Body => msg!(Body),
            };
            let detail = if result.snippet.is_empty() {
                result.tags.join(" · ")
            } else {
                result.snippet
            };
            selectable_row(
                v_stack((
                    h_stack((
                        text(result.title).style(move |style| {
                            style
                                .font_size(crate::ui::FONT_BODY as f32)
                                .color(palette.sidebar_ink)
                                .min_width(0.0)
                                .flex_shrink(1.0)
                                .text_ellipsis()
                                .selectable(false)
                        }),
                        empty().style(|style| style.flex_grow(1.0)),
                        text(kind).style(move |style| {
                            style
                                .font_size(crate::ui::FONT_CAPTION as f32)
                                .color(palette.sidebar_accent)
                                .flex_shrink(0.0)
                                .selectable(false)
                        }),
                    ))
                    .style(|style| style.width_full().min_width(0.0).items_center().gap(6.0)),
                    text(detail).style(move |style| {
                        style
                            .font_size(crate::ui::FONT_CAPTION as f32)
                            .color(palette.sidebar_muted)
                            .text_ellipsis()
                            .selectable(false)
                    }),
                ))
                .style(|style| style.width_full().gap(4.0)),
                move || {
                    let opened = row_model.borrow_mut().open_search_result(
                        generation,
                        &search_query.get_untracked(),
                        &relative_path,
                    );
                    if opened {
                        search_open.set(false);
                        search_query.set(String::new());
                        editor_focus_request.update(|value| *value = value.saturating_add(1));
                    }
                    revision.update(|value| *value += 1);
                    schedule_autosave(row_model.clone(), revision);
                },
            )
            .style(move |style| {
                let selected = search_selected.get() == row_index;
                style
                    .width_full()
                    .min_height(56.0)
                    .padding_vert(8.0)
                    .padding_horiz(8.0)
                    .background(if selected {
                        palette.sidebar_active
                    } else {
                        Color::TRANSPARENT
                    })
                    .color(palette.sidebar_ink)
                    .border_radius(6.0)
                    .hover(move |style| style.background(palette.sidebar_active))
            })
        },
    )
    .style(|style| style.flex_col().gap(2.0).width_full());
    let search_input_model = model.clone();
    let search_key_model = model.clone();
    let search_input = localized_input::LocalizedInput::new(search_query, i18n::Key::SearchNotes)
        .style(move |style| {
            text_input_affordance(style, palette.sidebar_muted, palette.sidebar_accent)
                .min_width(0.0)
                .height(32.0)
                .items_center()
                .flex_grow(1.0)
                .padding_horiz(10.0)
                .background(palette.sidebar_active)
                .color(palette.sidebar_ink)
                .border(1.0)
                .border_color(palette.sidebar_border)
                .border_radius(5.0)
                .font_size(crate::ui::FONT_BODY as f32)
        });
    let search_input_id = search_input.id();
    create_effect(move |_| {
        if search_open.get() {
            search_input_id.request_focus();
        }
    });
    let search_input = search_input.on_event(EventListener::KeyDown, move |event| {
        let Event::KeyDown(key_event) = event else {
            return EventPropagation::Continue;
        };
        match &key_event.key.logical_key {
            Key::Named(NamedKey::ArrowDown) => {
                let result_count = search_key_model.borrow().search_results.len();
                if result_count > 0 {
                    search_selected
                        .update(|value| *value = value.saturating_add(1).min(result_count - 1));
                }
                EventPropagation::Stop
            }
            Key::Named(NamedKey::ArrowUp) => {
                search_selected.update(|value| *value = value.saturating_sub(1));
                EventPropagation::Stop
            }
            Key::Named(NamedKey::Enter) => {
                let query = search_query.get_untracked();
                let target = {
                    let model = search_key_model.borrow();
                    model
                        .search_result_generation(&query)
                        .and_then(|generation| {
                            model
                                .search_results
                                .get(search_selected.get_untracked())
                                .map(|result| (generation, result.relative_path.clone()))
                        })
                };
                if let Some((generation, relative_path)) = target {
                    let opened = search_key_model.borrow_mut().open_search_result(
                        generation,
                        &query,
                        &relative_path,
                    );
                    if opened {
                        search_open.set(false);
                        search_query.set(String::new());
                        editor_focus_request.update(|value| *value = value.saturating_add(1));
                    }
                    revision.update(|value| *value += 1);
                    schedule_autosave(search_key_model.clone(), revision);
                }
                EventPropagation::Stop
            }
            _ => EventPropagation::Continue,
        }
    });
    let close_search_model = search_input_model.clone();
    let retry_search_model = search_input_model.clone();
    let retry_search_state_model = search_input_model.clone();
    let search_retry = icon_button(
        ButtonAction::Refresh.icon(),
        || tr!(RebuildSearch),
        IconButtonTone::Sidebar,
        palette,
        move || {
            let mut model = retry_search_model.borrow_mut();
            model.rebuild_search();
            revision.update(|value| *value += 1);
        },
    )
    .style(move |style| {
        revision.get();
        if retry_search_state_model.borrow().search_error.is_some() {
            style
        } else {
            style.hide()
        }
    });
    let search_controls = h_stack((
        search_input,
        search_retry,
        icon_button(
            ICON_CANCEL,
            || tr!(CloseSearch),
            IconButtonTone::Sidebar,
            palette,
            move || {
                search_open.set(false);
                search_query.set(String::new());
                close_search_model.borrow_mut().search_results.clear();
                editor_focus_request.update(|value| *value = value.saturating_add(1));
                revision.update(|value| *value += 1);
            },
        ),
    ))
    .style(move |style| {
        let style = style.width_full().items_center().gap(6.0);
        if search_open.get() {
            style
        } else {
            style.hide()
        }
    });
    let search_status_model = model.clone();
    let search_status = label(move || {
        revision.get();
        let model = search_status_model.borrow();
        if model.search_indexing {
            tr!(Indexing)
        } else if model.search_error.is_some() {
            tr!(SearchTemporarilyUnavailable)
        } else if search_query.get().trim().is_empty() {
            format!(
                "{}\n{}",
                tr!(SearchPrompt),
                tr!(SearchShortcut, "modifier" => i18n::shortcut_modifier())
            )
        } else if model.search_results.is_empty() {
            tr!(NoResults)
        } else {
            String::new()
        }
    })
    .style(move |style| {
        revision.get();
        let hidden = !search_open.get()
            || (!search_input_model.borrow().search_indexing
                && search_input_model.borrow().search_error.is_none()
                && !search_query.get().trim().is_empty()
                && !search_input_model.borrow().search_results.is_empty());
        let style = style
            .font_size(crate::ui::FONT_CAPTION as f32)
            .color(palette.sidebar_muted)
            .padding_horiz(4.0);
        if hidden { style.hide() } else { style }
    });
    // Search starts display:none, so prime one real transition to hidden while
    // it is off-screen. Floem then has HideBars applied before the first
    // visible search frame instead of briefly painting its default bar.
    let search_scrollbar_visible = create_rw_signal(true);
    let search_scrollbar_generation = create_rw_signal(0_u64);
    let search_scroll_origin = create_rw_signal(None::<Point>);
    let search_results = scroll(search_rows)
        .on_scroll(move |viewport| {
            let origin = viewport.origin();
            let previous = search_scroll_origin.get_untracked();
            search_scroll_origin.set(Some(origin));
            if previous.is_some_and(|previous| previous != origin) {
                show_scrollbar_temporarily(search_scrollbar_visible, search_scrollbar_generation);
            }
        })
        .style(move |style| {
            let style = style.width_full().min_height(0.0).flex_grow(1.0);
            if search_open.get() {
                style
            } else {
                style.hide()
            }
        })
        .scroll_style(move |style| {
            search_open.get();
            style.hide_bars(!search_scrollbar_visible.get())
        });
    create_effect(move |_| {
        search_open.get();
        for generation in [tree_scrollbar_generation, search_scrollbar_generation] {
            generation.set(generation.get_untracked().saturating_add(1));
        }
        tree_scrollbar_visible.set(false);
        search_scrollbar_visible.set(false);
        tree_scroll_origin.set(None);
        search_scroll_origin.set(None);
    });
    let content = v_stack((
        header,
        search_controls,
        search_status,
        search_results,
        tree,
        rail,
        empty().style(move |style| {
            style
                .flex_grow(1.0)
                .apply_if(!sidebar_state.get().collapsed, |style| style.hide())
        }),
        collapse,
    ))
    .style(move |style| {
        style
            .width(actual_width.get())
            .flex_shrink(0.0)
            .height_full()
            .min_height(0.0)
            .gap(10.0)
            .padding(12.0)
            .background(palette.sidebar)
            .color(palette.sidebar_ink)
    });
    let handle = sidebar_resize_handle(sidebar_width, actual_width, palette).style(move |style| {
        style.apply_if(
            sidebar_state.get().collapsed || window_size.get().width < 1000.0,
            |style| style.hide(),
        )
    });
    stack((content, handle)).style(move |style| {
        style
            .width(actual_width.get())
            .height_full()
            .min_height(0.0)
            .flex_shrink(0.0)
    })
}

/// Reactive inputs of the editor pane that are shared with the sidebar.
#[derive(Clone, Copy)]
struct EditorPanelSignals {
    tag_popover: TagPopoverSignals,
    sidebar_state: RwSignal<SidebarState>,
    search_open: RwSignal<bool>,
    note_find: NoteFindSignals,
    go_to_line: GoToLineSignals,
    editor_focus_request: RwSignal<u64>,
}

#[derive(Clone, Copy)]
struct NoteFindSignals {
    open: RwSignal<bool>,
    query: RwSignal<String>,
    selected: RwSignal<usize>,
    matches: RwSignal<Vec<ByteRange>>,
    focus_request: RwSignal<u64>,
}

#[derive(Clone)]
struct RssCardData {
    expanded: bool,
    hidden: bool,
    entry: RssEntry,
    unread: bool,
    selected: bool,
}

fn rss_title(label: String, ink: Color) -> impl IntoView {
    text(label).pointer_events(|| false).style(move |style| {
        style
            .width_full()
            .min_width(0.0)
            .font_size(ui::FONT_CARD)
            .line_height(1.25)
            .font_weight(floem::text::Weight::SEMIBOLD)
            .selectable(false)
            .font_family(ui::UI_FONT_FAMILY.to_owned())
            .color(ink)
    })
}

fn rss_article_link(
    label: String,
    on_press: impl Fn() + 'static,
    palette: Palette,
    ink: Color,
) -> impl IntoView {
    selectable_row(rss_title(label, ink), on_press)
        // The card selects on bubbling pointer events; the title handles its own
        // selection before opening so it must not also activate the card.
        .on_event(EventListener::PointerDown, |event| {
            if is_primary_pointer_down(event) {
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .style(move |style| {
            style
                .width_full()
                .min_width(0.0)
                .cursor(CursorStyle::Pointer)
                .border_radius(5.0)
                .focus_visible(|style| style.background(palette.accent_soft))
        })
}

/// Current summary of one subscription. Every feed control reads its state
/// through this one lookup instead of walking the subscription list again.
fn rss_subscription_summary(
    model: &Rc<RefCell<AppModel>>,
    item_id: &ItemId,
) -> Option<RssSubscriptionSummary> {
    model.borrow().workspace.as_ref().and_then(|workspace| {
        workspace
            .rss_subscriptions()
            .into_iter()
            .find(|summary| &summary.subscription.id == item_id)
    })
}

#[derive(Clone, Copy)]
struct RssToolbarSignals {
    filters_open: RwSignal<bool>,
    rename: ToolbarEditBar,
    categories: ToolbarEditBar,
}

/// One feed toolbar control, built from the action the engine declared.
fn rss_toolbar_control(
    action: ToolbarAction,
    model: Rc<RefCell<AppModel>>,
    item_id: ItemId,
    revision: RwSignal<u64>,
    signals: RssToolbarSignals,
    palette: Palette,
) -> AnyView {
    let state_model = model.clone();
    let state_id = item_id.clone();
    let subscription_state = move || {
        revision.get();
        rss_subscription_summary(&state_model, &state_id)
    };
    match action {
        ToolbarAction::Filters => {
            rss_filters::control(model, item_id, revision, signals.filters_open, palette)
        }
        ToolbarAction::Refresh => {
            let busy_model = model.clone();
            let busy_id = item_id.clone();
            toolbar_control(
                action,
                ToolbarSubject::Feed,
                palette,
                move || {
                    revision.get();
                    busy_model
                        .borrow()
                        .rss_refreshing
                        .contains(busy_id.as_str())
                },
                move || {
                    if model.borrow_mut().start_rss_refresh(item_id.clone()) {
                        schedule_rss_poll(model.clone(), revision);
                    }
                    revision.update(|value| *value = value.saturating_add(1));
                },
            )
            .into_any()
        }
        ToolbarAction::Rename => toolbar_control(
            action,
            ToolbarSubject::Feed,
            palette,
            move || signals.rename.open.get(),
            move || {
                if signals.rename.open.get_untracked() {
                    signals.rename.open.set(false);
                    return;
                }
                signals.rename.value.set(
                    subscription_state()
                        .map(|summary| summary.display_title)
                        .unwrap_or_default(),
                );
                signals.categories.open.set(false);
                signals.rename.open.set(true);
            },
        )
        .into_any(),
        ToolbarAction::Categories => toolbar_control(
            action,
            ToolbarSubject::Feed,
            palette,
            move || signals.categories.open.get(),
            move || {
                if signals.categories.open.get_untracked() {
                    signals.categories.open.set(false);
                    return;
                }
                signals.categories.value.set(
                    subscription_state()
                        .map(|summary| summary.subscription.categories.join(", "))
                        .unwrap_or_default(),
                );
                signals.rename.open.set(false);
                signals.categories.open.set(true);
            },
        )
        .into_any(),
        ToolbarAction::Pin => toolbar_control(
            action,
            ToolbarSubject::Feed,
            palette,
            move || subscription_state().is_some_and(|summary| summary.subscription.pinned),
            move || {
                model.borrow_mut().toggle_selected_rss_pinned();
                revision.update(|value| *value = value.saturating_add(1));
                schedule_autosave(model.clone(), revision);
            },
        )
        .into_any(),
        ToolbarAction::Favorite => toolbar_control(
            action,
            ToolbarSubject::Feed,
            palette,
            move || subscription_state().is_some_and(|summary| summary.subscription.favorited),
            move || {
                model.borrow_mut().toggle_selected_rss_favorited();
                revision.update(|value| *value = value.saturating_add(1));
                schedule_autosave(model.clone(), revision);
            },
        )
        .into_any(),
        ToolbarAction::Delete | ToolbarAction::Restore => {
            let deleted = matches!(action, ToolbarAction::Restore);
            toolbar_control(
                action,
                ToolbarSubject::Feed,
                palette,
                || false,
                move || {
                    model.borrow_mut().set_selected_rss_deleted(!deleted);
                    revision.update(|value| *value = value.saturating_add(1));
                    schedule_autosave(model.clone(), revision);
                },
            )
            .into_any()
        }
    }
}

fn rss_toolbar_controls(
    declared_actions: &[ToolbarAction],
    deleted: bool,
    model: Rc<RefCell<AppModel>>,
    item_id: ItemId,
    revision: RwSignal<u64>,
    signals: RssToolbarSignals,
    palette: Palette,
) -> AnyView {
    // Floem can deliver a queued DynamicContainer update after the parent feed
    // scope has been disposed. Its form signals share that scope: do not create
    // controls whose initial styles would read them after switching to a file.
    if signals.rename.open.try_get_untracked().is_none() {
        return empty().into_any();
    }

    let controls = visible_toolbar_actions(declared_actions, deleted)
        .into_iter()
        .map(|action| {
            rss_toolbar_control(
                action,
                model.clone(),
                item_id.clone(),
                revision,
                signals,
                palette,
            )
        })
        .collect::<Vec<_>>();
    h_stack_from_iter(controls)
        .style(|style| style.items_center().gap(TOOLBAR_ACTION_GAP_PX))
        .into_any()
}

fn rss_panel(
    model: Rc<RefCell<AppModel>>,
    item_id: ItemId,
    revision: RwSignal<u64>,
    palette: Palette,
) -> AnyView {
    let feed_focus_request = create_rw_signal(0_u64);
    let scroll_target = create_rw_signal(None::<Point>);
    let viewport_height = create_rw_signal(0.0_f64);
    let signals = RssToolbarSignals {
        filters_open: create_rw_signal(false),
        rename: ToolbarEditBar {
            open: create_rw_signal(false),
            value: create_rw_signal(String::new()),
            label: i18n::Key::NewTitle,
            placeholder: i18n::Key::FeedTitle,
        },
        categories: ToolbarEditBar {
            open: create_rw_signal(false),
            value: create_rw_signal(String::new()),
            label: i18n::Key::CategoriesPlaceholder,
            placeholder: i18n::Key::CategoriesExample,
        },
    };

    let title_model = model.clone();
    let title_id = item_id.clone();
    let title_click_model = model.clone();
    let title_click_id = item_id.clone();
    // The engine declares which controls its items support; the toolbar only
    // decides which of the delete and restore pair matches the current state.
    let declared_actions = model
        .borrow()
        .workspace
        .as_ref()
        .map(WorkspaceSession::rss_toolbar_actions)
        .unwrap_or_default();
    let actions_model = model.clone();
    let actions_state_model = model.clone();
    let actions_id = item_id.clone();
    let actions_state_id = item_id.clone();
    let deleted = floem::reactive::create_memo(move |_| {
        revision.get();
        rss_subscription_summary(&actions_state_model, &actions_state_id)
            .is_some_and(|summary| summary.subscription.deleted)
    });
    let actions = dyn_container(
        move || deleted.get(),
        move |deleted| {
            rss_toolbar_controls(
                &declared_actions,
                deleted,
                actions_model.clone(),
                actions_id.clone(),
                revision,
                signals,
                palette,
            )
        },
    );

    let toolbar = ui::content_header(
        ICON_RSS,
        move || {
            revision.get();
            rss_subscription_summary(&title_model, &title_id)
                .map(|summary| summary.display_title).unwrap_or_else(|| tr!(RssFeed))
        },
        actions,
        Some(Rc::new(move || {
            signals.rename.value.set(rss_subscription_summary(&title_click_model, &title_click_id)
                .map(|summary| summary.display_title).unwrap_or_default());
            signals.categories.open.set(false);
            signals.rename.open.set(true);
        })),
        palette,
    );

    let rename_model = model.clone();
    let rename_form = toolbar_edit_bar(signals.rename, palette, move || {
        if rename_model
            .borrow_mut()
            .rename_selected_rss(&signals.rename.value.get_untracked())
        {
            signals.rename.open.set(false);
        }
        revision.update(|value| *value = value.saturating_add(1));
        schedule_autosave(rename_model.clone(), revision);
    });

    let categories_model = model.clone();
    let categories_form = toolbar_edit_bar(signals.categories, palette, move || {
        let categories = parsed_category_list(&signals.categories.value.get_untracked());
        if categories_model
            .borrow_mut()
            .set_selected_rss_categories(categories)
        {
            signals.categories.open.set(false);
        }
        revision.update(|value| *value = value.saturating_add(1));
        schedule_autosave(categories_model.clone(), revision);
    });

    let entries_model = model.clone();
    let entries_id = item_id.clone();
    let card_model = model.clone();
    let last_revealed = Rc::new(RefCell::new(None::<String>));
    let cards = dyn_stack(
        move || {
            revision.get();
            let model = entries_model.borrow();
            let selected = model.selected_rss_entry.as_deref();
            model
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.rss_feed(&entries_id).ok())
                .map(|(feed, state)| {
                    feed.entries
                        .into_iter()
                        .map(|entry| RssCardData {
                            hidden: state.hidden(&entry.id),
                            expanded: model.expanded_rss_entry.as_deref()
                                == Some(entry.id.as_str()),
                            unread: !state.read_entry_ids.contains(&entry.id),
                            selected: selected == Some(entry.id.as_str()),
                            entry,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        },
        |card| {
            (
                card.entry.clone(),
                card.unread,
                card.selected,
                card.hidden,
                card.expanded,
            )
        },
        move |card| {
            let card_top = create_rw_signal(0.0_f64);
            let entry_id = card.entry.id.clone();
            let select_model = card_model.clone();
            let open_model = card_model.clone();
            let excerpt = rss_card::excerpt(&card.entry.summary);
            let original_url = card
                .entry
                .link
                .as_deref()
                .and_then(rss_card::article_url)
                .or_else(|| excerpt.continuation.clone());
            let ink = if card.unread {
                palette.ink
            } else {
                rss_card::read_title_ink(palette.paper)
            };
            let body_ink = if card.unread {
                palette.ink
            } else {
                palette.ink2
            };
            let published = card.entry.published.clone().or(card.entry.updated.clone());
            let author = card.entry.author.clone();
            let metadata = move || {
                let date = rss_card::date_label(published.as_deref());
                [
                    author
                        .as_deref()
                        .map(str::trim)
                        .filter(|author| !author.is_empty()),
                    (!date.is_empty()).then_some(date.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ")
            };
            let summary_layout = excerpt.layout(body_ink);
            let select_entry = Rc::new(move || {
                let selected = select_model.borrow_mut().select_rss_entry(&entry_id);
                scroll_target.set(Some(Point::new(0.0, card_top.get_untracked())));
                feed_focus_request.update(|value| *value = value.saturating_add(1));
                revision.update(|value| *value = value.saturating_add(1));
                selected
            });
            let title = if card.hidden {
                let select_title = select_entry.clone();
                rss_article_link(
                    card.entry.title.clone(),
                    move || {
                        select_title();
                    },
                    palette,
                    ink,
                )
                .into_any()
            } else if let Some(url) = original_url {
                let select_title = select_entry.clone();
                rss_article_link(
                    card.entry.title.clone(),
                    move || {
                        if select_title() {
                            open_model.borrow_mut().error = open_rss_original(&url)
                                .err()
                                .map(|error| error.to_string().into());
                            revision.update(|value| *value = value.saturating_add(1));
                        }
                    },
                    palette,
                    ink,
                )
                .into_any()
            } else {
                rss_title(card.entry.title.clone(), ink).into_any()
            };
            let select_pointer = select_entry.clone();
            let view = v_stack((
                h_stack((
                    title.style(|style| style.min_width(0.0).flex_grow(1.0).flex_shrink(1.0)),
                    empty().style(move |style| {
                        style
                            .size(6.0, 6.0)
                            .margin_top(6.0)
                            .flex_shrink(0.0)
                            .border_radius(3.0)
                            .background(if card.unread {
                                palette.accent
                            } else {
                                Color::TRANSPARENT
                            })
                    }),
                ))
                .style(|style| style.width_full().min_width(0.0).items_start().gap(12.0)),
                label(metadata)
                    .pointer_events(|| false)
                    .style(move |style| {
                        style
                            .width_full()
                            .min_width(0.0)
                            .font_size(ui::FONT_CAPTION)
                            .line_height(1.4)
                            .font_family(ui::UI_FONT_FAMILY.to_owned())
                            .color(palette.ink2)
                            .apply_if(card.hidden && !card.expanded, |s| s.hide())
                    }),
                floem::views::rich_text(move || summary_layout.clone())
                    .pointer_events(|| false)
                    .style(move |style| {
                        style
                            .width_full()
                            .min_width(0.0)
                            .apply_if(card.hidden && !card.expanded, |s| s.hide())
                    }),
            ))
            .keyboard_navigable()
            .on_event(EventListener::PointerDown, move |event| {
                if is_primary_pointer_down(event) {
                    select_pointer();
                    EventPropagation::Stop
                } else {
                    EventPropagation::Continue
                }
            })
            .on_event(EventListener::KeyDown, move |event| {
                if is_keyboard_activation(event) {
                    select_entry();
                    EventPropagation::Stop
                } else {
                    EventPropagation::Continue
                }
            })
            .style(move |style| {
                style
                    .width_full()
                    .padding(24.0)
                    .gap(12.0)
                    .background(palette.paper)
                    .border(1.0)
                    .border_color(if card.selected {
                        palette.accent
                    } else {
                        palette.divider
                    })
                    .border_radius(8.0)
            });
            let last_revealed = last_revealed.clone();
            let reveal_entry_id = card.entry.id.clone();
            view.on_resize(move |rect| {
                card_top.set(rect.y0);
                // Selected cards are remounted by the list's key. Their bounds
                // are only available after layout, not during construction.
                if card.selected && last_revealed.borrow().as_ref() != Some(&reveal_entry_id) {
                    *last_revealed.borrow_mut() = Some(reveal_entry_id.clone());
                    // Coordinates are relative to the card stack, keeping the
                    // content's 20px inset above the selected card.
                    scroll_target.set(Some(Point::new(0.0, rect.y0)));
                }
            })
        },
    )
    .style(|style| style.width_full().flex_col().gap(16.0));
    let list = scroll(v_stack((cards,)).style(move |style| {
        style
            .width_full()
            .padding(20.0)
            // Leave room to top-align even the final card in a short feed.
            .padding_bottom(viewport_height.get().max(20.0))
    }))
    .scroll_to(move || scroll_target.get())
    .style(|style| style.width_full().min_height(0.0).flex_grow(1.0))
    .on_resize(move |rect| viewport_height.set(rect.height()));
    let status_model = model.clone();
    let status_style_model = model.clone();
    let status = label(move || {
        revision.get();
        status_model
            .borrow()
            .error
            .as_ref()
            .map(i18n::user_error_text)
            .unwrap_or_default()
    })
    .style(move |style| {
        revision.get();
        let style = style
            .width_full()
            .min_height(30.0)
            .padding_horiz(20.0)
            .items_center()
            .font_size(crate::ui::FONT_CAPTION as f32)
            .color(Color::rgb8(190, 72, 72))
            .background(palette.paper)
            .border_top(1.0)
            .border_color(palette.divider);
        if status_style_model.borrow().error.is_some() {
            style
        } else {
            style.hide()
        }
    });
    let panel = v_stack((toolbar, rename_form, categories_form, list, status))
        .style(move |style| {
            style
                .width_full()
                .min_width(0.0)
                .min_height(0.0)
                .flex_shrink(1.0)
                .height_full()
                .background(palette.canvas)
        })
        .keyboard_navigable()
        .into_any();
    let focus_id = panel.id();
    create_effect(move |mounted: Option<()>| {
        feed_focus_request.get();
        let editing = signals.rename.open.get()
            || signals.categories.open.get()
            || signals.filters_open.get();
        if !editing {
            if mounted.is_some() {
                // The panel survives form closure and card replacement. Queue
                // focus now so the hidden field cannot consume the next key.
                focus_id.request_focus();
            } else {
                // Only initial activation needs to wait for the panel to mount.
                exec_after(Duration::from_millis(10), move |_| {
                    if signals.rename.open.try_get_untracked() == Some(false)
                        && signals.categories.open.try_get_untracked() == Some(false)
                        && signals.filters_open.try_get_untracked() == Some(false)
                    {
                        focus_id.request_focus();
                    }
                });
            }
        }
    });
    panel
}

fn main_content_panel(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: EditorPanelSignals,
    context: PanelContext,
    settings: SettingsPageSignals,
) -> AnyView {
    let editor_state_model = model.clone();
    let editor =
        editor_panel(model.clone(), revision, signals, context.clone()).style(move |style| {
            revision.get();
            if editor_state_model
                .borrow()
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_engine_item)
                .is_some()
            {
                style.hide()
            } else {
                style
            }
        });
    let feed_state_model = model.clone();
    let feed_visibility_model = model.clone();
    let feed_model = model.clone();
    let feed_palette = context.palette;
    let feed = dyn_stack(
        move || {
            revision.get();
            let model = feed_state_model.borrow();
            model
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_rss)
                .cloned()
                .map(|id| (model.rss_session, id))
                .into_iter()
                .collect::<Vec<_>>()
        },
        Clone::clone,
        move |(_, item_id)| rss_panel(feed_model.clone(), item_id, revision, feed_palette),
    )
    .style(move |style| {
        revision.get();
        let style = style
            .width_full()
            .min_width(0.0)
            .flex_shrink(1.0)
            .height_full();
        if feed_visibility_model
            .borrow()
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_rss)
            .is_some()
        {
            style
        } else {
            style.hide()
        }
    });
    let chat_state = model.clone();
    let chat_model = model.clone();
    let chat_visible = model;
    let chat = dyn_stack(
        move || {
            revision.get();
            let model = chat_state.borrow();
            model
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_engine_item)
                .filter(|(e, _)| e == &stillus_chat::engine_id())
                .map(|(_, id)| (model.rss_session, id.clone()))
                .into_iter()
                .collect::<Vec<_>>()
        },
        Clone::clone,
        move |(_, id)| chat_view::panel(chat_model.clone(), id, revision, settings, feed_palette),
    )
    .style(move |style| {
        revision.get();
        let visible = chat_visible
            .borrow()
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_engine_item)
            .is_some_and(|(e, _)| e == &stillus_chat::engine_id());
        style
            .width_full()
            .min_width(0.0)
            .flex_shrink(1.0)
            .height_full()
            .apply_if(!visible, |s| s.hide())
    });
    stack((editor, feed, chat))
        .style(|style| {
            style
                .flex_basis(0.0)
                .flex_grow(1.0)
                .min_width(0.0)
                .height_full()
                .min_height(0.0)
        })
        .into_any()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NoteFindKey {
    target: DocumentTarget,
    content_revision: u64,
    query: String,
    editor_columns: usize,
    editor_rows: usize,
}

#[derive(Clone, Copy)]
struct GoToLineSignals {
    open: RwSignal<bool>,
    query: RwSignal<String>,
    error: RwSignal<Option<GoToLineError>>,
    focus_request: RwSignal<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoToLineError {
    Empty,
    Invalid,
    OutOfRange { maximum: usize },
}

impl fmt::Display for GoToLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str(&tr!(EnterLine)),
            Self::Invalid => formatter.write_str(&tr!(PositiveLine)),
            Self::OutOfRange { maximum } => {
                write!(
                    formatter,
                    "{}",
                    tr!(LineRange, "maximum" => maximum.to_string())
                )
            }
        }
    }
}

fn parse_go_to_line(query: &str, maximum: usize) -> Result<usize, GoToLineError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(GoToLineError::Empty);
    }
    let line = query.parse::<usize>().map_err(|_| GoToLineError::Invalid)?;
    if line == 0 || line > maximum {
        return Err(GoToLineError::OutOfRange { maximum });
    }
    Ok(line - 1)
}

fn close_go_to_line(signals: GoToLineSignals) {
    signals.open.set(false);
    signals.query.set(String::new());
    signals.error.set(None);
}

fn open_go_to_line(
    model: &Rc<RefCell<AppModel>>,
    signals: GoToLineSignals,
    search_open: RwSignal<bool>,
    note_find: NoteFindSignals,
    tag_popover: TagPopoverSignals,
) -> bool {
    if !document_is_open(model) {
        return false;
    }
    search_open.set(false);
    close_note_find(note_find);
    close_tag_popover(tag_popover);
    signals.query.set(String::new());
    signals.error.set(None);
    signals.open.set(true);
    signals
        .focus_request
        .update(|value| *value = value.saturating_add(1));
    true
}

fn go_to_line(model: &mut AppModel, query: &str) -> Result<usize, GoToLineError> {
    let document = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .ok_or(GoToLineError::OutOfRange { maximum: 0 })?;
    let maximum = document.line_count();
    let target = parse_go_to_line(query, maximum)?;
    let offset = document
        .viewport(ViewportRequest {
            first_line: target,
            visible_lines: 1,
            overscan_lines: 0,
        })
        .ok()
        .and_then(|snapshot| snapshot.lines.first().map(|line| line.start.get()))
        .ok_or(GoToLineError::OutOfRange { maximum })?;
    model.apply(EditorCommand::SetCaret {
        offset,
        extend: false,
    });
    model.viewport_first_line = target
        .saturating_sub(model.editor_rows.max(1) / 2)
        .min(model.max_viewport_first_line());
    let visible_bottom =
        EDITOR_PADDING_Y_PX + model.editor_rows.max(1) as f64 * EDITOR_LINE_HEIGHT_PX;
    if caret_geometry(model).is_none_or(|(_, y)| y >= visible_bottom) {
        model.viewport_first_line = target.min(model.max_viewport_first_line());
    }
    Ok(target)
}

fn submit_go_to_line(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: GoToLineSignals,
    editor_focus_request: RwSignal<u64>,
) {
    let query = signals.query.get_untracked();
    let result = {
        let mut model = model.borrow_mut();
        go_to_line(&mut model, &query)
    };
    match result {
        Ok(_) => {
            close_go_to_line(signals);
            revision.update(|value| *value = value.saturating_add(1));
            editor_focus_request.update(|value| *value = value.saturating_add(1));
        }
        Err(error) => signals.error.set(Some(error)),
    }
}

fn go_to_line_prompt(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: GoToLineSignals,
    editor_focus_request: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let error_clear = signals.error;
    create_effect(move |_| {
        signals.query.get();
        error_clear.set(None);
    });

    let key_model = model.clone();
    let input =
        localized_input::LocalizedInput::new(signals.query, i18n::Key::Line).style(move |style| {
            let has_error = signals.error.get().is_some();
            text_input_affordance(style, palette.muted, palette.accent)
                .width(112.0)
                .height(36.0)
                .items_center()
                .padding_horiz(10.0)
                .background(palette.canvas)
                .color(palette.ink)
                .border(1.0)
                .border_color(if has_error {
                    palette.danger
                } else {
                    palette.divider
                })
                .border_radius(5.0)
                .font_size(crate::ui::FONT_BODY as f32)
        });
    let input_id = input.id();
    create_effect(move |_| {
        let focus_request = signals.focus_request.get();
        if signals.open.get() && focus_request > 0 {
            input_id.request_focus();
        }
    });
    let input = input.on_event(EventListener::KeyDown, move |event| {
        let Event::KeyDown(key_event) = event else {
            return EventPropagation::Continue;
        };
        match &key_event.key.logical_key {
            Key::Named(NamedKey::Enter) => {
                submit_go_to_line(&key_model, revision, signals, editor_focus_request);
                EventPropagation::Stop
            }
            Key::Named(NamedKey::Escape) => {
                close_go_to_line(signals);
                editor_focus_request.update(|value| *value = value.saturating_add(1));
                EventPropagation::Stop
            }
            _ => EventPropagation::Continue,
        }
    });

    let range_model = model.clone();
    let submit_model = model;
    let card = v_stack((
        label(|| tr!(GoToLine)).style(|style| style.font_size(crate::ui::FONT_CARD as f32)),
        h_stack((
            input,
            label(move || {
                revision.get();
                let maximum = range_model
                    .borrow()
                    .workspace
                    .as_ref()
                    .and_then(WorkspaceSession::document)
                    .map_or(0, |document| document.line_count());
                tr!(OfMaximum , "maximum" => maximum)
            })
            .style(move |style| {
                style
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .color(palette.muted)
            }),
            empty().style(|style| style.flex_grow(1.0)),
            dialog_button(
                ButtonAction::Custom(ICON_ARROW_DOWN),
                msg!(Go),
                IconButtonTone::Primary,
                palette,
                move || {
                    submit_go_to_line(&submit_model, revision, signals, editor_focus_request);
                },
            ),
        ))
        .style(|style| style.width_full().items_center().gap(8.0)),
        label(move || {
            signals
                .error
                .get()
                .map_or_else(|| tr!(GoKeys), |error| error.to_string())
        })
        .style(move |style| {
            style
                .min_height(16.0)
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(if signals.error.get().is_some() {
                    palette.danger
                } else {
                    palette.muted
                })
        }),
    ))
    .style(move |style| {
        style
            .width(340.0)
            .gap(12.0)
            .padding(16.0)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(8.0)
    });

    container(card).style(move |style| {
        let style = style
            .absolute()
            .size_full()
            .items_center()
            .justify_center()
            .z_index(10)
            .background(Color::rgba8(24, 29, 36, 36));
        if signals.open.get() {
            style
        } else {
            style.hide()
        }
    })
}

fn close_note_find(signals: NoteFindSignals) {
    signals.open.set(false);
    signals.query.set(String::new());
    signals.selected.set(0);
    signals.matches.set(Vec::new());
}

fn open_note_find(
    model: &Rc<RefCell<AppModel>>,
    signals: NoteFindSignals,
    search_open: RwSignal<bool>,
    tag_popover: TagPopoverSignals,
    go_to_line: GoToLineSignals,
) -> bool {
    if !local_search_is_available(model) {
        return false;
    }
    search_open.set(false);
    close_tag_popover(tag_popover);
    close_go_to_line(go_to_line);
    signals.open.set(true);
    signals
        .focus_request
        .update(|value| *value = value.saturating_add(1));
    true
}

fn select_note_find_match(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: NoteFindSignals,
    index: usize,
) {
    let Some(range) = signals.matches.get_untracked().get(index).copied() else {
        return;
    };
    signals.selected.set(index);
    let mut model = model.borrow_mut();
    model.apply(EditorCommand::SetSelection {
        anchor: range.start().get(),
        focus: range.end().get(),
    });
    model.reveal_editor_selection(range);
    drop(model);
    revision.update(|value| *value += 1);
}

fn step_note_find_match(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: NoteFindSignals,
    backwards: bool,
) {
    let count = signals.matches.get_untracked().len();
    if count == 0 {
        return;
    }
    let current = signals.selected.get_untracked().min(count - 1);
    let next = if backwards {
        current.checked_sub(1).unwrap_or(count - 1)
    } else {
        (current + 1) % count
    };
    select_note_find_match(model, revision, signals, next);
}

#[derive(Clone, Copy)]
struct TagPopoverSignals {
    open: RwSignal<bool>,
    target_path: RwSignal<Option<PathBuf>>,
    query: RwSignal<String>,
    highlighted: RwSignal<Option<usize>>,
    hovered_tag: RwSignal<Option<String>>,
    trigger_pointer_down: RwSignal<bool>,
}

fn close_tag_popover(signals: TagPopoverSignals) {
    signals.open.set(false);
    signals.target_path.set(None);
    signals.query.set(String::new());
    signals.highlighted.set(None);
    signals.hovered_tag.set(None);
}

fn editor_panel(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    signals: EditorPanelSignals,
    context: PanelContext,
) -> impl IntoView {
    let PanelContext { security, palette } = context;
    let EditorPanelSignals {
        tag_popover,
        sidebar_state,
        search_open,
        note_find,
        go_to_line,
        editor_focus_request,
    } = signals;
    let pin_model = model.clone();
    let pin_label_model = model.clone();
    let pin_disabled_model = model.clone();
    let pin_revision = revision;
    let favorite_model = model.clone();
    let favorite_label_model = model.clone();
    let favorite_disabled_model = model.clone();
    let favorite_revision = revision;
    let deleted_state_model = model.clone();
    let deleted_action_model = model.clone();
    let deleted_revision = revision;
    let editor_model = model.clone();
    let editor_font_family = model.borrow().editor_font_family.clone();
    let editor_id_model = model.clone();
    let editor_click_model = model.clone();
    let editor_selection_model = model.clone();
    let editor_menu_model = model.clone();
    let pointer_model = model.clone();
    let caret_model = model.clone();
    let caret_visible = create_rw_signal(false);
    let caret_focused = create_rw_signal(false);
    let caret_generation = create_rw_signal(0_u64);
    let status_model = model.clone();
    let status_tooltip_model = model.clone();
    let status_color_model = model.clone();
    let actions_state_model = model.clone();
    let retry_model = model.clone();
    let recover_model = model.clone();
    let recover_security = security.clone();
    let reload_model = model.clone();
    let tag_target_model = model.clone();
    create_effect(move |_| {
        revision.get();
        if !tag_popover.open.get() {
            return;
        }
        let current_path = tag_target_model
            .borrow()
            .workspace
            .as_ref()
            .and_then(|workspace| {
                workspace
                    .selected_note()
                    .and_then(|index| workspace.notes().get(index))
                    .map(|note| note.path.clone())
            });
        if current_path != tag_popover.target_path.get_untracked() {
            close_tag_popover(tag_popover);
        }
    });
    let find_effect_model = model.clone();
    let find_key = Rc::new(RefCell::new(None::<NoteFindKey>));
    create_effect(move |_| {
        revision.get();
        if !note_find.open.get() {
            find_key.borrow_mut().take();
            return;
        }
        let query = note_find.query.get();
        let find_state = {
            let model = find_effect_model.borrow();
            model.workspace.as_ref().and_then(|workspace| {
                let document = workspace.document()?;
                let key = NoteFindKey {
                    target: document.target().clone(),
                    content_revision: document.content_revision(),
                    query: query.clone(),
                    editor_columns: model.editor_columns,
                    editor_rows: model.editor_rows,
                };
                Some((
                    key,
                    workspace.search_selected_document(&query, NOTE_FIND_MATCH_LIMIT),
                ))
            })
        };
        let Some((key, matches)) = find_state else {
            find_key.borrow_mut().take();
            close_note_find(note_find);
            return;
        };
        let matches = match matches {
            Ok(matches) => matches,
            Err(error) => {
                find_key.borrow_mut().take();
                close_note_find(note_find);
                find_effect_model.borrow_mut().error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                revision.update(|value| *value += 1);
                return;
            }
        };
        if find_key.borrow().as_ref() == Some(&key) {
            return;
        }
        *find_key.borrow_mut() = Some(key);
        note_find.matches.set(matches);
        note_find.selected.set(0);
        if !query.is_empty() && !note_find.matches.get_untracked().is_empty() {
            select_note_find_match(&find_effect_model, revision, note_find, 0);
        }
    });
    let recovery_actions = dyn_container(
        move || {
            revision.get();
            let model = actions_state_model.borrow();
            let Some(workspace) = model.workspace.as_ref() else {
                return (false, false, false);
            };
            let retry = workspace
                .document()
                .is_some_and(|document| matches!(document.save_status(), SaveStatus::Error { .. }))
                || (model.deferred_note_action_pending() && !model.deferred_note_action_busy());
            let reload = workspace.document().is_some_and(|document| {
                matches!(document.save_status(), SaveStatus::Conflict { .. })
            });
            // A recovery backup exists transiently during every autosave, so
            // the recover affordance would flash on each keystroke. Suppress it
            // while a save is dirty or in flight; it stays available once the
            // document settles (e.g. an artifact left by a previous session).
            let save_in_flight = workspace.document().is_some_and(|document| {
                matches!(
                    document.save_status(),
                    SaveStatus::Dirty { .. } | SaveStatus::Saving { .. }
                )
            });
            let recover = !save_in_flight
                && match workspace.selected_target() {
                    Some(DocumentTarget::WorkspaceNote(index)) => workspace
                        .notes()
                        .get(index)
                        .is_some_and(|note| note.recovery_available),
                    Some(DocumentTarget::ExternalFile { engine_id, item_id }) => workspace
                        .external_files()
                        .iter()
                        .find(|file| file.engine_id == engine_id && file.item_id == item_id)
                        .is_some_and(|file| file.recovery_available),
                    None => false,
                };
            (retry, recover, reload)
        },
        move |(retry, recover, reload)| {
            let mut actions = Vec::new();
            if retry {
                let retry_model = retry_model.clone();
                actions.push(icon_button(
                    ButtonAction::Refresh.icon(),
                    || tr!(RetrySave),
                    IconButtonTone::Status,
                    palette,
                    move || {
                        let should_retry = {
                            let mut model = retry_model.borrow_mut();
                            if model.deferred_note_action_pending() {
                                model.retry_deferred_note_action()
                            } else {
                                model.retry_save()
                            }
                        };
                        if should_retry {
                            revision.update(|value| *value += 1);
                            schedule_autosave(retry_model.clone(), revision);
                        }
                    },
                ));
            }
            if recover {
                let recover_model = recover_model.clone();
                let recover_security = recover_security.clone();
                actions.push(icon_button(
                    ICON_RECOVER,
                    || tr!(RestoreUnsaved),
                    IconButtonTone::Status,
                    palette,
                    move || {
                        let result = recover_model.borrow_mut().restore_selected_recovery();
                        match result {
                            Ok(_) => {
                                schedule_autosave(recover_model.clone(), revision);
                            }
                            Err(CoreError::MasterPasswordRequired) => {
                                let note_index = recover_model
                                    .borrow()
                                    .workspace
                                    .as_ref()
                                    .and_then(WorkspaceSession::selected_note);
                                if let Some(note_index) = note_index {
                                    recover_security
                                        .open(PasswordDialogKind::UnlockForRecovery { note_index });
                                }
                            }
                            Err(error) => {
                                recover_model.borrow_mut().error = Some(UiText::Failure {
                                    details: error.to_string(),
                                });
                            }
                        }
                        revision.update(|value| *value += 1);
                    },
                ));
            }
            if reload {
                let reload_model = reload_model.clone();
                actions.push(icon_button(
                    ICON_DISK_VERSION,
                    || tr!(LoadDisk),
                    IconButtonTone::Status,
                    palette,
                    move || {
                        let result = reload_model
                            .borrow_mut()
                            .discard_local_and_reload()
                            .map_err(|error| error.to_string());
                        if let Err(error) = result {
                            reload_model.borrow_mut().error = Some(UiText::Failure {
                                details: error.to_string(),
                            });
                        } else {
                            schedule_autosave(reload_model.clone(), revision);
                        }
                        revision.update(|value| *value += 1);
                    },
                ));
            }
            h_stack_from_iter(actions).style(|style| style.items_center().gap(6.0).flex_shrink(0.0))
        },
    );
    let editor_scrollbar_visible = create_rw_signal(false);
    let editor_scrollbar_generation = create_rw_signal(0_u64);
    let editor_padding_model = model.clone();
    let line_number_model = model.clone();
    let line_number_width_model = model.clone();
    let line_number_font_family = editor_font_family.clone();
    let editor_text = label(move || {
        revision.get();
        render_editor(&editor_model.borrow())
    })
    .style(move |style| {
        revision.get();
        let (padding_x, _, _) = editor_horizontal_metrics(&editor_padding_model.borrow());
        style
            .width_full()
            .min_height_full()
            .padding_vert(EDITOR_PADDING_Y_PX)
            .padding_left(padding_x)
            .padding_right(EDITOR_PADDING_X_PX)
            .color(palette.ink)
            .font_family(editor_font_family.clone())
            .font_size(EDITOR_FONT_SIZE_PX)
            .line_height(EDITOR_LINE_HEIGHT_MULTIPLIER)
            .text_clip()
            // The editor owns caret, selection and focus. Floem's built-in
            // label selection would paint a second grey overlay, steal focus
            // and capture the pointer during drags.
            .selectable(false)
    })
    .on_event_stop(EventListener::PointerWheel, move |event| {
        if let Event::PointerWheel(pointer) = event {
            if pointer_model
                .borrow_mut()
                .scroll_editor_wheel(pointer.delta.y)
            {
                show_scrollbar_temporarily(editor_scrollbar_visible, editor_scrollbar_generation);
                revision.update(|value| *value += 1);
            }
        }
    });
    let line_numbers = label(move || {
        revision.get();
        render_editor_line_numbers(&line_number_model.borrow())
    })
    .pointer_events(|| false)
    .style(move |style| {
        revision.get();
        let model = line_number_width_model.borrow();
        let line_count = model
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::document)
            .map_or(1, |document| document.line_count());
        style
            .absolute()
            .inset_left(0.0)
            .inset_top(0.0)
            .width(editor_line_number_width(
                line_count,
                model.editor_character_width,
            ))
            .padding_left(EDITOR_LINE_NUMBER_PADDING_LEFT_PX)
            .padding_vert(EDITOR_PADDING_Y_PX)
            .color(palette.muted)
            .font_family(line_number_font_family.clone())
            .font_size(EDITOR_FONT_SIZE_PX)
            .line_height(EDITOR_LINE_HEIGHT_MULTIPLIER)
            .text_clip()
            .selectable(false)
    });
    let caret = empty().pointer_events(|| false).style(move |style| {
        revision.get();
        let caret_model = caret_model.borrow();
        let geometry = caret_geometry(&caret_model);
        let selection_is_caret = caret_model
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::document)
            .is_none_or(|document| document.selection().is_caret());
        let visible = caret_focused.get() && caret_visible.get() && selection_is_caret;
        let style = style
            .absolute()
            .width(2.0)
            .height(EDITOR_CARET_HEIGHT_PX)
            .border_radius(1.0)
            .background(palette.accent);
        match geometry {
            Some((x, y)) if visible => style.inset_left(x).inset_top(y),
            _ => style.hide(),
        }
    });
    let selection_highlights = dyn_container(
        move || {
            revision.get();
            editor_selection_rects(&editor_selection_model.borrow())
        },
        move |rects| {
            stack_from_iter(rects.into_iter().map(move |rect| {
                empty().pointer_events(|| false).style(move |style| {
                    style
                        .absolute()
                        .inset_left(rect.x)
                        .inset_top(rect.y)
                        .width(rect.width)
                        .height(EDITOR_SELECTION_HEIGHT_PX)
                        .border_radius(2.0)
                        .background(palette.accent_soft)
                })
            }))
            .pointer_events(|| false)
            .style(|style| style.absolute().width_full().height_full())
        },
    )
    .pointer_events(|| false)
    .style(|style| style.absolute().width_full().height_full());
    let editor_content = stack((selection_highlights, line_numbers, editor_text, caret))
        .style(|style| style.width_full().min_height_full());
    let resize_model = model.clone();
    let editor_surface = scroll(editor_content)
        .style(move |style| {
            style
                .width_full()
                .min_height(0.0)
                .flex_grow(1.0)
                .background(palette.paper)
                .cursor(CursorStyle::Text)
        })
        .on_resize(move |rect| {
            let changed = resize_model
                .borrow_mut()
                .update_editor_metrics(rect.width(), rect.height());
            if changed {
                revision.update(|value| *value += 1);
            }
        });
    let drag_active = create_rw_signal(false);
    let drag_model = model.clone();
    let click_visible = caret_visible;
    let click_focused = caret_focused;
    let click_generation = caret_generation;
    let key_visible = caret_visible;
    let key_focused = caret_focused;
    let key_generation = caret_generation;
    let focus_visible = caret_visible;
    let focus_focused = caret_focused;
    let focus_generation = caret_generation;
    let blur_visible = caret_visible;
    let blur_focused = caret_focused;
    let blur_generation = caret_generation;
    let editor_surface = PrimaryPointerView::new(editor_surface, move |pointer| {
        let command = editor_command_for_pointer(&editor_click_model.borrow(), pointer);
        if let Some(command) = command {
            editor_click_model.borrow_mut().apply(command);
            revision.update(|value| *value += 1);
        }
        drag_active.set(pointer.count == 1);
        restart_caret_blink(click_visible, click_focused, click_generation);
    })
    .capture_pointer()
    .style(move |style| {
        style
            .width_full()
            .min_height(0.0)
            .flex_grow(1.0)
            .background(palette.paper)
            .cursor(CursorStyle::Text)
    });
    let editor_focus_id = editor_surface.id();
    create_effect(move |_| {
        if editor_focus_request.get() > 0 {
            editor_focus_id.request_focus();
        }
    });
    let editor_surface = editor_surface
        .keyboard_navigable()
        .on_event(EventListener::PointerMove, move |event| {
            if let Event::PointerMove(pointer) = event
                && drag_active.get_untracked()
            {
                let command = editor_drag_command_for_point(
                    &drag_model.borrow(),
                    pointer.pos.x,
                    pointer.pos.y,
                );
                if let Some(command) = command {
                    drag_model.borrow_mut().apply(command);
                    revision.update(|value| *value += 1);
                }
            }
            EventPropagation::Continue
        })
        .on_event(EventListener::PointerUp, move |_| {
            drag_active.set(false);
            EventPropagation::Continue
        })
        .on_event_stop(EventListener::KeyDown, move |event| {
            if popover_handle_escape(event) {
                return;
            }
            if let Event::KeyDown(key_event) = event {
                if is_go_to_line_shortcut(key_event) {
                    open_go_to_line(
                        &editor_id_model,
                        go_to_line,
                        search_open,
                        note_find,
                        tag_popover,
                    );
                } else if is_note_find_shortcut(key_event) {
                    open_note_find(
                        &editor_id_model,
                        note_find,
                        search_open,
                        tag_popover,
                        go_to_line,
                    );
                } else if is_search_shortcut(key_event) {
                    search_open.set(true);
                } else {
                    restart_caret_blink(key_visible, key_focused, key_generation);
                    handle_key_event(key_event, &editor_id_model, revision);
                }
            }
        })
        .on_event_stop(EventListener::FocusGained, move |_| {
            focus_focused.set(true);
            restart_caret_blink(focus_visible, focus_focused, focus_generation);
            revision.update(|value| *value += 1);
        })
        .on_event_stop(EventListener::FocusLost, move |_| {
            blur_focused.set(false);
            blur_visible.set(false);
            blur_generation.update(|generation| *generation = generation.saturating_add(1));
            revision.update(|value| *value += 1);
        });
    let editor_surface = context_menu_view(editor_surface, palette, move || {
        editor_context_menu(editor_menu_model.clone(), revision, editor_focus_id)
    })
    .style(|style| style.size_full());
    let editor_scrollbar_model = model.clone();
    let editor_scrollbar = empty().pointer_events(|| false).style(move |style| {
        revision.get();
        let geometry = editor_scrollbar_thumb(&editor_scrollbar_model.borrow());
        let style = style
            .absolute()
            .inset_right(EDITOR_SCROLLBAR_INSET_PX)
            .width(EDITOR_SCROLLBAR_WIDTH_PX)
            .border_radius(EDITOR_SCROLLBAR_WIDTH_PX / 2.0)
            .background(palette.scrollbar)
            .z_index(5);
        match geometry {
            Some((top, height)) if editor_scrollbar_visible.get() => {
                style.inset_top(top).height(height)
            }
            _ => style.hide(),
        }
    });
    let go_to_line_prompt = go_to_line_prompt(
        model.clone(),
        revision,
        go_to_line,
        editor_focus_request,
        palette,
    );
    let protected_overlay = protected_placeholder_card(model.clone(), revision, palette);
    let editor_body = stack((
        editor_surface,
        editor_scrollbar,
        protected_overlay,
        go_to_line_prompt,
    ))
    .style(|style| style.width_full().min_height(0.0).flex_grow(1.0));
    let protection_menu_open = create_rw_signal(false);
    let protection_state_model = model.clone();
    let protection_action_model = model.clone();
    let protection_popover_model = model.clone();
    let protection_security = security.clone();
    let protection_action = dyn_container(
        move || {
            revision.get();
            let model = protection_state_model.borrow();
            protection_action_state(&model)
        },
        move |state| match state {
            // Keep the slot so the neighbouring actions never shift while a
            // security operation is in flight or no note is selected.
            ProtectionActionState::None => empty()
                .style(|style| style.size(BUTTON_SIZE_PX, BUTTON_SIZE_PX))
                .into_any(),
            ProtectionActionState::Decrypting => enabled_icon_button(
                state.icon().expect("decrypting action has an icon"),
                || tr!(Decrypting),
                IconButtonTone::Secondary,
                palette,
                || false,
                || {},
            )
            .into_any(),
            ProtectionActionState::Protect => {
                let action_model = protection_action_model.clone();
                let action_security = protection_security.clone();
                icon_button(
                    state.icon().expect("protect action has an icon"),
                    || tr!(ProtectNote),
                    IconButtonTone::Secondary,
                    palette,
                    move || {
                        close_tag_popover(tag_popover);
                        let dialog = action_model
                            .borrow()
                            .workspace
                            .as_ref()
                            .map(protection_password_dialog);
                        if let Some(dialog) = dialog {
                            action_security.open(dialog);
                        }
                    },
                )
                .into_any()
            }
            ProtectionActionState::Lock => icon_toggle_button(
                state.icon().expect("lock action has an icon"),
                || tr!(LockNote),
                palette,
                move || protection_menu_open.get(),
                move || {
                    close_tag_popover(tag_popover);
                    protection_menu_open.set(!protection_menu_open.get_untracked());
                },
            )
            .into_any(),
            ProtectionActionState::Unlock { note_index } => {
                let action_security = protection_security.clone();
                icon_button(
                    state.icon().expect("unlock action has an icon"),
                    || tr!(UnlockNote),
                    IconButtonTone::Secondary,
                    palette,
                    move || {
                        action_security.open(PasswordDialogKind::Unlock { note_index });
                    },
                )
                .into_any()
            }
            ProtectionActionState::UnlockKnown { note_index } => {
                let action_model = protection_action_model.clone();
                icon_button(
                    state
                        .icon()
                        .expect("known-password unlock action has an icon"),
                    || tr!(UnlockNote),
                    IconButtonTone::Secondary,
                    palette,
                    move || {
                        action_model.borrow_mut().open_note(note_index);
                        revision.update(|value| *value += 1);
                        schedule_autosave(action_model.clone(), revision);
                    },
                )
                .into_any()
            }
        },
    );
    let protection_action = anchored_popover(
        protection_action,
        protection_menu_open,
        PROTECTION_POPOVER_WIDTH_PX,
        4.0,
        true,
        move || {
            protection_popover(
                protection_popover_model.clone(),
                revision,
                protection_menu_open,
                palette,
            )
        },
    );
    let find_input = localized_input::LocalizedInput::new(note_find.query, i18n::Key::FindDocument)
        .style(move |style| {
            text_input_affordance(style, palette.muted, palette.accent)
                .min_width(104.0)
                .height(32.0)
                .items_center()
                .flex_grow(1.0)
                .padding_horiz(10.0)
                .background(palette.canvas)
                .color(palette.ink)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(5.0)
                .font_size(crate::ui::FONT_BODY as f32)
        });
    let find_input_id = find_input.id();
    create_effect(move |_| {
        let focus_request = note_find.focus_request.get();
        if note_find.open.get() && focus_request > 0 {
            find_input_id.request_focus();
        }
    });
    let find_key_model = model.clone();
    let find_input = find_input.on_event(EventListener::KeyDown, move |event| {
        let Event::KeyDown(key_event) = event else {
            return EventPropagation::Continue;
        };
        if key_event.key.logical_key == Key::Named(NamedKey::Enter) {
            step_note_find_match(
                &find_key_model,
                revision,
                note_find,
                key_event.modifiers.shift(),
            );
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    });
    let previous_find_model = model.clone();
    let next_find_model = model.clone();
    let find_bar = h_stack((
        find_input,
        label(move || {
            let count = note_find.matches.get().len();
            if note_find.query.get().is_empty() {
                String::new()
            } else if count == 0 {
                tr!(NoMatches)
            } else if count == NOTE_FIND_MATCH_LIMIT {
                let position = note_find.selected.get().min(count - 1) + 1;
                tr!(MoreMatches , "position" => position, "maximum" => NOTE_FIND_MATCH_LIMIT)
            } else {
                tr!(MatchPosition , "value" => note_find.selected.get().min(count - 1) + 1, "count" => count)
            }
        })
        .style(move |style| {
            style
                .min_width(44.0)
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
                .text_ellipsis()
        }),
        icon_button(
            ICON_ARROW_UP,
            || tr!(PreviousMatch),
            IconButtonTone::Status,
            palette,
            move || step_note_find_match(&previous_find_model, revision, note_find, true),
        ),
        icon_button(
            ICON_ARROW_DOWN,
            || tr!(NextMatch),
            IconButtonTone::Status,
            palette,
            move || step_note_find_match(&next_find_model, revision, note_find, false),
        ),
        icon_button(
            ICON_CANCEL,
            || tr!(CloseFind),
            IconButtonTone::Status,
            palette,
            move || {
                close_note_find(note_find);
                editor_focus_request.update(|value| *value = value.saturating_add(1));
            },
        ),
    ))
    .style(move |style| {
        let style = style
            .width_full()
            .min_width(0.0)
            .height(48.0)
            .flex_shrink(0.0)
            .padding_horiz(20.0)
            .background(palette.paper)
            .border_bottom(1.0)
            .border_color(palette.divider)
            .items_center()
            .gap(4.0)
            .flex_shrink(1.0);
        if note_find.open.get() {
            style
        } else {
            style.hide()
        }
    });
    let find_button_model = model.clone();
    let find_button_disabled_model = model.clone();
    let find_action = enabled_icon_toggle_button(
        ButtonAction::Search.icon(),
        || tr!(FindShortcut, "modifier" => i18n::shortcut_modifier()),
        palette,
        move || {
            revision.get();
            local_search_is_available(&find_button_disabled_model)
        },
        move || note_find.open.get(),
        move || {
            open_note_find(
                &find_button_model,
                note_find,
                search_open,
                tag_popover,
                go_to_line,
            );
        },
    );
    let tag_button_disabled_model = model.clone();
    let tag_button_action_model = model.clone();
    let tag_button = enabled_toolbar_control(
        ToolbarAction::Categories,
        ToolbarSubject::Note,
        palette,
        move || {
            revision.get();
            selected_note_is_ready(&tag_button_disabled_model)
        },
        move || tag_popover.open.get(),
        move || {
            close_go_to_line(go_to_line);
            tag_popover.trigger_pointer_down.set(true);
            exec_after(Duration::from_millis(0), move |_| {
                tag_popover.trigger_pointer_down.set(false);
            });
            if tag_popover.open.get_untracked() {
                close_tag_popover(tag_popover);
                return;
            }
            let target_path = tag_button_action_model
                .borrow()
                .workspace
                .as_ref()
                .and_then(|workspace| {
                    workspace
                        .selected_note()
                        .and_then(|index| workspace.notes().get(index))
                        .map(|note| note.path.clone())
                });
            if let Some(path) = target_path {
                tag_popover.target_path.set(Some(path));
                tag_popover.query.set(String::new());
                tag_popover.highlighted.set(None);
                tag_popover.hovered_tag.set(None);
                tag_popover.open.set(true);
            }
        },
    );
    let tag_button_id = tag_button.id();
    let tag_popover_model = model.clone();
    let tag_action = anchored_popover(
        tag_button,
        tag_popover.open,
        TAG_POPOVER_WIDTH_PX,
        TAG_POPOVER_GAP_PX,
        true,
        move || {
            tag_popover_card(
                tag_popover_model.clone(),
                revision,
                sidebar_state,
                tag_popover,
                tag_button_id,
                palette,
            )
        },
    );
    let metadata_visibility_model = model.clone();
    let title_model = model.clone();
    let title_click_model = model.clone();
    let title_save_model = model.clone();
    let title_target_model = model.clone();
    let title_edit = ToolbarEditBar {
        open: create_rw_signal(false),
        value: create_rw_signal(String::new()),
        label: i18n::Key::NewTitle,
        placeholder: i18n::Key::NewTitle,
    };
    create_effect(move |previous: Option<Option<DocumentTarget>>| {
        revision.get();
        let target = title_target_model
            .borrow()
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_target);
        if previous.is_some_and(|previous| previous != target) {
            title_edit.open.set(false);
        }
        target
    });
    let rename_form = toolbar_edit_bar(title_edit, palette, move || {
        let accepted = {
            let mut model = title_save_model.borrow_mut();
            model.edit_note_title(&title_edit.value.get_untracked())
                || model.title_edit_pending(&title_edit.value.get_untracked())
        };
        if accepted {
            title_edit.open.set(false);
            editor_focus_request.update(|value| *value += 1);
        }
        revision.update(|value| *value += 1);
        schedule_autosave(title_save_model.clone(), revision);
    });
    let dismiss_error_model = model.clone();
    let error_visibility_model = model.clone();
    let error_icon_model = model.clone();
    let operation_busy_model = model.clone();
    let pin_busy_model = model.clone();
    let favorite_busy_model = model.clone();
    v_stack((
        ui::content_header(
            ICON_NOTE,
            move || {
                revision.get();
                let model = title_model.borrow();
                model
                    .workspace
                    .as_ref()
                    .and_then(|workspace| {
                        workspace
                            .document()
                            .map(|document| document.title().to_owned())
                            .or_else(|| {
                                workspace
                                    .selected_note()
                                    .and_then(|index| workspace.notes().get(index))
                                    .map(|note| note.title.clone())
                            })
                    })
                    .unwrap_or_else(|| "Stillus".to_owned())
            },
            h_stack((
                find_action,
                h_stack((
                    tag_action,
                    protection_action,
                    busy_note_toolbar_control(
                        ToolbarAction::Pin,
                        palette,
                        move || {
                            revision.get();
                            selected_note_is_ready(&pin_disabled_model)
                        },
                        move || {
                            revision.get();
                            selected_note_flag(&pin_label_model, |note| note.pinned)
                        },
                        move || {
                            revision.get();
                            pin_busy_model.borrow().deferred_note_action_busy()
                        },
                        move || {
                            pin_model.borrow_mut().toggle_pinned_selected();
                            pin_revision.update(|value| *value += 1);
                            schedule_autosave(pin_model.clone(), pin_revision);
                        },
                    ),
                    busy_note_toolbar_control(
                        ToolbarAction::Favorite,
                        palette,
                        move || {
                            revision.get();
                            selected_note_is_ready(&favorite_disabled_model)
                        },
                        move || {
                            revision.get();
                            selected_note_flag(&favorite_label_model, |note| note.favorited)
                        },
                        move || {
                            revision.get();
                            favorite_busy_model.borrow().deferred_note_action_busy()
                        },
                        move || {
                            favorite_model.borrow_mut().toggle_favorited_selected();
                            favorite_revision.update(|value| *value += 1);
                            schedule_autosave(favorite_model.clone(), favorite_revision);
                        },
                    ),
                    dyn_container(
                        move || {
                            revision.get();
                            selected_note_flag(&deleted_state_model, |note| note.deleted)
                        },
                        move |deleted| {
                            let action_model = deleted_action_model.clone();
                            let busy_model = deleted_action_model.clone();
                            busy_note_toolbar_control(
                                if deleted {
                                    ToolbarAction::Restore
                                } else {
                                    ToolbarAction::Delete
                                },
                                palette,
                                || true,
                                || false,
                                move || {
                                    revision.get();
                                    busy_model.borrow().deferred_note_action_busy()
                                },
                                move || {
                                    action_model.borrow_mut().set_deleted_selected(!deleted);
                                    deleted_revision.update(|value| *value += 1);
                                    schedule_autosave(action_model.clone(), deleted_revision);
                                },
                            )
                            .into_any()
                        },
                    ),
                ))
                .style(move |style| {
                    revision.get();
                    let external = metadata_visibility_model
                        .borrow()
                        .workspace
                        .as_ref()
                        .and_then(WorkspaceSession::selected_target)
                        .is_some_and(|target| {
                            matches!(target, DocumentTarget::ExternalFile { .. })
                        });
                    let style = style.items_center().gap(TOOLBAR_ACTION_GAP_PX);
                    if external { style.hide() } else { style }
                }),
            ))
            .style(|style| {
                style
                    .items_center()
                    .gap(TOOLBAR_ACTION_GAP_PX)
                    .flex_shrink(0.0)
            }),
            Some(Rc::new(move || {
                let title = title_click_model
                    .borrow()
                    .workspace
                    .as_ref()
                    .and_then(WorkspaceSession::document)
                    .filter(|document| !document.is_external())
                    .map(|document| document.title().to_owned());
                if let Some(title) = title {
                    title_edit.value.set(title);
                    close_note_find(note_find);
                    title_edit.open.set(true);
                }
            })),
            palette,
        ),
        rename_form,
        find_bar,
        editor_body,
        h_stack((
            anchored_tooltip(
                label(move || {
                    revision.get();
                    editor_status(&status_model.borrow())
                }),
                Rc::new(move || {
                    revision.get();
                    let model = status_tooltip_model.borrow();
                    let mut status = editor_status(&model);
                    if let Some(details) = model.error.as_ref().and_then(i18n::safe_error_details) {
                        status.push('\n');
                        status.push_str(&details);
                    }
                    status
                }),
                palette,
            )
            .style(move |style| {
                revision.get();
                let model = status_color_model.borrow();
                let is_error = model.error.is_some()
                    || model
                        .workspace
                        .as_ref()
                        .and_then(WorkspaceSession::document)
                        .is_some_and(|document| {
                            matches!(
                                document.save_status(),
                                SaveStatus::Error { .. } | SaveStatus::Conflict { .. }
                            )
                        });
                style
                    .min_width(0.0)
                    .flex_shrink(1.0)
                    .text_ellipsis()
                    .font_size(crate::ui::FONT_CAPTION as f32)
                    .color(if is_error {
                        palette.danger
                    } else {
                        palette.muted
                    })
            }),
            empty().style(|style| style.flex_grow(1.0)),
            recovery_actions,
            svg(ICON_WARNING).style(move |s| {
                revision.get();
                s.size(16.0, 16.0)
                    .color(palette.danger)
                    .flex_shrink(0.0)
                    .apply_if(error_icon_model.borrow().error.is_none(), |s| s.hide())
            }),
            label(move || {
                revision.get();
                if operation_busy_model.borrow().deferred_note_action_busy() {
                    tr!(WaitingAutosave)
                } else {
                    String::new()
                }
            })
            .style(move |s| s.color(palette.muted).flex_shrink(0.0)),
            icon_button(
                ButtonAction::Close.icon(),
                || tr!(Close),
                IconButtonTone::Status,
                palette,
                move || {
                    let mut model = dismiss_error_model.borrow_mut();
                    model.error = None;
                    model.cancel_close_after_save();
                    model.cancel_deferred_note_action();
                    drop(model);
                    revision.update(|value| *value += 1);
                },
            )
            .style(move |s| {
                revision.get();
                s.apply_if(error_visibility_model.borrow().error.is_none(), |s| {
                    s.hide()
                })
            }),
        ))
        .style(move |style| {
            style
                .height(32.0)
                .min_height(32.0)
                .max_height(32.0)
                .flex_shrink(0.0)
                .width_full()
                .items_center()
                .gap(8.0)
                .padding_horiz(20.0)
                .background(palette.canvas)
                .border_top(1.0)
                .border_color(palette.divider)
        }),
    ))
    .style(|style| {
        style
            .height_full()
            .min_width(0.0)
            .min_height(0.0)
            .flex_shrink(1.0)
            .flex_grow(1.0)
    })
}

fn handle_key_event(
    key_event: &floem::keyboard::KeyEvent,
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
) {
    let shortcut = (key_event.modifiers.meta() || key_event.modifiers.control())
        && !altgr_text(key_event.modifiers, key_event.key.text.as_deref());
    let word_modifier =
        key_event.modifiers.alt() || (key_event.modifiers.control() && !key_event.modifiers.meta());
    let shift = key_event.modifiers.shift();
    let command = if altgr_text(key_event.modifiers, key_event.key.text.as_deref()) {
        key_event
            .key
            .text
            .as_ref()
            .map(|text| EditorCommand::Insert(text.to_string()))
    } else if let Some(command) =
        platform_edit_command(&key_event.key.logical_key, key_event.modifiers)
    {
        Some(command)
    } else if is_toggle_task_done_shortcut(key_event.modifiers, key_event.key.physical_key) {
        Some(EditorCommand::ToggleTaskDone)
    } else {
        match &key_event.key.logical_key {
            Key::Named(NamedKey::ArrowLeft) if key_event.modifiers.meta() => {
                Some(EditorCommand::MoveLineStart { extend: shift })
            }
            Key::Named(NamedKey::ArrowRight) if key_event.modifiers.meta() => {
                Some(EditorCommand::MoveLineEnd { extend: shift })
            }
            Key::Named(NamedKey::ArrowUp) if key_event.modifiers.meta() => {
                Some(EditorCommand::MoveDocumentStart { extend: shift })
            }
            Key::Named(NamedKey::ArrowDown) if key_event.modifiers.meta() => {
                Some(EditorCommand::MoveDocumentEnd { extend: shift })
            }
            Key::Named(NamedKey::ArrowLeft) if word_modifier => {
                Some(EditorCommand::MoveWordLeft { extend: shift })
            }
            Key::Named(NamedKey::ArrowRight) if word_modifier => {
                Some(EditorCommand::MoveWordRight { extend: shift })
            }
            Key::Named(NamedKey::Home) => Some(EditorCommand::MoveLineStart { extend: shift }),
            Key::Named(NamedKey::End) => Some(EditorCommand::MoveLineEnd { extend: shift }),
            Key::Named(NamedKey::ArrowLeft) => Some(EditorCommand::MoveLeft { extend: shift }),
            Key::Named(NamedKey::ArrowRight) => Some(EditorCommand::MoveRight { extend: shift }),
            Key::Named(NamedKey::ArrowUp) => Some(EditorCommand::MoveUp { extend: shift }),
            Key::Named(NamedKey::ArrowDown) => Some(EditorCommand::MoveDown { extend: shift }),
            Key::Named(NamedKey::Backspace) => Some(EditorCommand::Backspace),
            Key::Named(NamedKey::Delete) => Some(EditorCommand::DeleteForward),
            Key::Named(NamedKey::Enter) if !shortcut => {
                Some(EditorCommand::Insert("\n".to_owned()))
            }
            Key::Named(NamedKey::Tab) if !shortcut => {
                Some(EditorCommand::Insert("    ".to_owned()))
            }
            Key::Named(NamedKey::PageUp) => {
                let mut model = model.borrow_mut();
                let page = model.editor_rows.max(1) as isize;
                model.editor_wheel_remainder = 0.0;
                model.scroll_lines(-page);
                None
            }
            Key::Named(NamedKey::PageDown) => {
                let mut model = model.borrow_mut();
                let page = model.editor_rows.max(1) as isize;
                model.editor_wheel_remainder = 0.0;
                model.scroll_lines(page);
                None
            }
            Key::Character(character) if shortcut => match character.to_lowercase().as_str() {
                "a" => Some(EditorCommand::SelectAll),
                "c" => Some(EditorCommand::Copy),
                "x" => Some(EditorCommand::Cut),
                "z" if shift => Some(EditorCommand::Redo),
                "z" => Some(EditorCommand::Undo),
                "v" => Clipboard::get_contents().ok().map(EditorCommand::Paste),
                _ => None,
            },
            _ if !shortcut && !key_event.modifiers.control() => key_event
                .key
                .text
                .as_ref()
                .map(|text| EditorCommand::Insert(text.to_string())),
            _ => None,
        }
    };
    if let Some(command) = command {
        execute_editor_command(model, revision, command);
    } else {
        revision.update(|value| *value += 1);
    }
}

fn altgr_text(modifiers: Modifiers, text: Option<&str>) -> bool {
    modifiers.control()
        && modifiers.alt()
        && !modifiers.meta()
        && text.is_some_and(|text| !text.is_empty() && text.chars().all(|c| !c.is_control()))
}

fn platform_edit_command(key: &Key, modifiers: Modifiers) -> Option<EditorCommand> {
    if cfg!(target_os = "macos") || !modifiers.control() || modifiers.alt() || modifiers.meta() {
        return None;
    }
    let extend = modifiers.shift();
    match key {
        Key::Named(NamedKey::Home) => Some(EditorCommand::MoveDocumentStart { extend }),
        Key::Named(NamedKey::End) => Some(EditorCommand::MoveDocumentEnd { extend }),
        Key::Character(value) if value.eq_ignore_ascii_case("y") => Some(EditorCommand::Redo),
        _ => None,
    }
}

fn is_toggle_task_done_shortcut(modifiers: Modifiers, physical_key: PhysicalKey) -> bool {
    modifiers == Modifiers::ALT && physical_key == KeyCode::KeyD
}

fn execute_editor_command(
    model: &Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    command: EditorCommand,
) {
    if let Some(contents) = model.borrow_mut().apply(command) {
        let _ = Clipboard::set_contents(contents);
    }
    revision.update(|value| *value += 1);
    schedule_autosave(model.clone(), revision);
}

fn editor_context_menu(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    editor_focus_id: ViewId,
) -> Vec<MenuEntry> {
    let has_clipboard_text = Clipboard::get_contents().is_ok_and(|contents| !contents.is_empty());
    let state = editor_menu_state(&model.borrow(), has_clipboard_text);

    let cut_model = model.clone();
    let copy_model = model.clone();
    let paste_model = model;
    vec![
        MenuEntry::action(
            ButtonAction::Cut.icon(),
            || tr!(Cut),
            move || state.can_cut_or_copy,
            move || {
                execute_editor_command(&cut_model, revision, EditorCommand::Cut);
                editor_focus_id.request_focus();
            },
        ),
        MenuEntry::action(
            ButtonAction::Copy.icon(),
            || tr!(Copy),
            move || state.can_cut_or_copy,
            move || {
                execute_editor_command(&copy_model, revision, EditorCommand::Copy);
                editor_focus_id.request_focus();
            },
        ),
        MenuEntry::action(
            ButtonAction::Paste.icon(),
            || tr!(Paste),
            move || state.can_paste,
            move || {
                if let Ok(contents) = Clipboard::get_contents() {
                    execute_editor_command(&paste_model, revision, EditorCommand::Paste(contents));
                }
                editor_focus_id.request_focus();
            },
        ),
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorMenuState {
    can_cut_or_copy: bool,
    can_paste: bool,
}

fn editor_menu_state(model: &AppModel, has_clipboard_text: bool) -> EditorMenuState {
    let can_cut_or_copy = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .is_some_and(|document| !document.selection().is_caret());
    EditorMenuState {
        can_cut_or_copy,
        can_paste: has_clipboard_text,
    }
}

fn restart_caret_blink(
    visible: RwSignal<bool>,
    focused: RwSignal<bool>,
    generation: RwSignal<u64>,
) {
    generation.update(|value| *value = value.saturating_add(1));
    let expected_generation = generation.get_untracked();
    visible.set(true);
    schedule_caret_blink_phase(visible, focused, generation, expected_generation, false);
}

fn schedule_caret_blink_phase(
    visible: RwSignal<bool>,
    focused: RwSignal<bool>,
    generation: RwSignal<u64>,
    expected_generation: u64,
    next_visible: bool,
) {
    exec_after(Duration::from_millis(CARET_BLINK_MS), move |_| {
        let Some(is_focused) = focused.try_get_untracked() else {
            return;
        };
        let Some(current_generation) = generation.try_get_untracked() else {
            return;
        };
        if !is_focused || current_generation != expected_generation {
            return;
        }
        visible.set(next_visible);
        schedule_caret_blink_phase(
            visible,
            focused,
            generation,
            expected_generation,
            !next_visible,
        );
    });
}

fn editor_viewport(model: &AppModel) -> Option<stillus_core::ViewportSnapshot> {
    model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)?
        .viewport(ViewportRequest {
            first_line: model.viewport_first_line,
            visible_lines: model.editor_rows.max(1),
            overscan_lines: 0,
        })
        .ok()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EditorSelectionRect {
    x: f64,
    y: f64,
    width: f64,
}

struct EditorLayout {
    snapshot: stillus_core::ViewportSnapshot,
    geometry: EditorTextGeometry,
}

impl EditorLayout {
    fn row_text(&self, row: usize) -> Option<&str> {
        self.geometry.row_text(row)
    }

    fn line_text(&self, row: usize) -> Option<&str> {
        let row = self.geometry.rows().get(row)?;
        self.snapshot
            .lines
            .get(row.line_slot)
            .map(|line| line.text.as_str())
    }
}

fn build_editor_geometry(
    model: &AppModel,
    snapshot: &stillus_core::ViewportSnapshot,
    max_rows: usize,
    apply_viewport_skip: bool,
) -> Option<EditorTextGeometry> {
    let (origin_x, content_width, _) = editor_horizontal_metrics(model);
    let lines = snapshot
        .lines
        .iter()
        .map(|line| GeometryLine {
            line_index: line.line_index,
            document_start: line.start.get(),
            document_end: line.end.get(),
            text: line.text.as_str(),
            truncated: line.truncated,
        })
        .collect::<Vec<_>>();
    EditorTextGeometry::build(
        &lines,
        GeometryConfig {
            font_family: model.editor_font_family.clone(),
            font_size: EDITOR_FONT_SIZE_PX as f32,
            line_height: EDITOR_LINE_HEIGHT_PX as f32,
            content_width: content_width as f32,
            tab_width: 4,
            origin_x,
            origin_y: EDITOR_PADDING_Y_PX,
            top_reserved_rows: 0,
            first_line_skip_rows: if apply_viewport_skip {
                model.viewport_first_visual_row
            } else {
                0
            },
            max_rows,
            caret_height: EDITOR_CARET_HEIGHT_PX,
            selection_height: EDITOR_SELECTION_HEIGHT_PX,
            selection_marker_width: model.editor_character_width / 2.0,
        },
    )
    .ok()
}

fn editor_layout(model: &AppModel) -> Option<EditorLayout> {
    let snapshot = editor_viewport(model)?;
    let geometry = build_editor_geometry(model, &snapshot, MAX_GEOMETRY_ROWS, true)?;
    Some(EditorLayout { snapshot, geometry })
}

fn editor_selection_rects(model: &AppModel) -> Vec<EditorSelectionRect> {
    let Some(document) = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
    else {
        return Vec::new();
    };
    let selection = document.selection().normalized();
    if selection.is_empty() {
        return Vec::new();
    }
    let Some(layout) = editor_layout(model) else {
        return Vec::new();
    };
    layout
        .geometry
        .selection_rects(selection.start().get()..selection.end().get())
        .into_iter()
        .map(|rect| EditorSelectionRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
        })
        .collect()
}

fn document_line_for_offset(model: &AppModel, offset: usize) -> Option<usize> {
    let document = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)?;
    if offset > document.len_bytes() {
        return None;
    }
    let mut lower = 0_usize;
    let mut upper = document.line_count();
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        let start = document
            .viewport(ViewportRequest {
                first_line: middle,
                visible_lines: 1,
                overscan_lines: 0,
            })
            .ok()?
            .lines
            .first()?
            .start
            .get();
        if start <= offset {
            lower = middle.saturating_add(1);
        } else {
            upper = middle;
        }
    }
    Some(lower.saturating_sub(1))
}

fn editor_selection_first_visual_row(model: &AppModel, selection: ByteRange) -> Option<usize> {
    let layout = editor_layout(model)?;
    layout
        .geometry
        .selection_rects(selection.start().get()..selection.end().get())
        .first()
        .map(|rect| rect.row)
}

fn editor_selection_is_fully_visible(model: &AppModel, selection: ByteRange) -> bool {
    if selection.is_empty() {
        return false;
    }
    let Some(start_line) = document_line_for_offset(model, selection.start().get()) else {
        return false;
    };
    let Some(end_line) = document_line_for_offset(model, selection.end().get().saturating_sub(1))
    else {
        return false;
    };
    let Some(layout) = editor_layout(model) else {
        return false;
    };
    if !layout
        .snapshot
        .lines
        .iter()
        .any(|line| line.line_index == start_line)
        || !layout
            .snapshot
            .lines
            .iter()
            .any(|line| line.line_index == end_line)
    {
        return false;
    }
    let rects = layout
        .geometry
        .selection_rects(selection.start().get()..selection.end().get());
    if rects.is_empty() {
        return false;
    }
    let top = EDITOR_PADDING_Y_PX;
    let bottom = top + model.editor_rows.max(1) as f64 * EDITOR_LINE_HEIGHT_PX;
    rects
        .iter()
        .all(|rect| rect.y >= top && rect.y + rect.height <= bottom)
}

fn editor_command_for_pointer(
    model: &AppModel,
    pointer: &PointerInputEvent,
) -> Option<EditorCommand> {
    match pointer.count {
        0 | 1 => editor_command_for_point(
            model,
            pointer.pos.x,
            pointer.pos.y,
            pointer.modifiers.shift(),
        ),
        2 => editor_word_command_for_point(model, pointer.pos.x, pointer.pos.y),
        // Floem counts up to four rapid clicks before wrapping, so every
        // click after the third keeps the native triple-click line selection.
        _ => editor_line_command_for_point(model, pointer.pos.y),
    }
}

/// Triple-click selects the whole document line under the pointer together
/// with its line break, so typing replaces the paragraph like a native text
/// view. A viewport-truncated line selects only its rendered prefix.
fn editor_line_command_for_point(model: &AppModel, y: f64) -> Option<EditorCommand> {
    let layout = editor_layout(model)?;
    let (origin_x, _, _) = editor_horizontal_metrics(model);
    let row = layout.geometry.hit_test_caret(origin_x, y)?.row;
    let line_slot = layout.geometry.rows().get(row)?.line_slot;
    let line = layout.snapshot.lines.get(line_slot)?;
    Some(EditorCommand::SetSelection {
        anchor: line.start.get(),
        focus: line.end.get(),
    })
}

fn editor_word_command_for_point(model: &AppModel, x: f64, y: f64) -> Option<EditorCommand> {
    let layout = editor_layout(model)?;
    let hit = layout.geometry.hit_test_glyph(x, y)?;
    let row = layout.geometry.rows().get(hit.row)?;
    let line_text = layout.line_text(hit.row)?;
    let relative = hit.document_offset.saturating_sub(row.document_start);
    let range = word_range_in_text(line_text, relative).ok()?;
    Some(EditorCommand::SetSelection {
        anchor: row.document_start.saturating_add(range.start().get()),
        focus: row.document_start.saturating_add(range.end().get()),
    })
}

fn editor_command_for_point(
    model: &AppModel,
    x: f64,
    y: f64,
    extend: bool,
) -> Option<EditorCommand> {
    let layout = editor_layout(model)?;
    let hit = layout.geometry.hit_test_caret(x, y)?;
    Some(EditorCommand::SetCaret {
        offset: hit.document_offset,
        extend,
    })
}

/// Pointer drag extends the selection only when the target differs from the
/// current focus, so hover-only movement never re-renders the viewport.
fn editor_drag_command_for_point(model: &AppModel, x: f64, y: f64) -> Option<EditorCommand> {
    let focus = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)?
        .selection()
        .focus()
        .get();
    match editor_command_for_point(model, x, y, true)? {
        EditorCommand::SetCaret { offset, .. } if offset == focus => None,
        command => Some(command),
    }
}

fn caret_geometry(model: &AppModel) -> Option<(f64, f64)> {
    let document = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)?;
    let layout = editor_layout(model)?;
    let cursor = document.selection().focus().get();
    let cursor_line = document.cursor_line().ok()?;
    let caret = layout.geometry.caret(cursor_line, cursor)?;
    Some((caret.x, caret.y))
}

/// Editor states of a protected note that replace the text surface with a
/// card instead of rendering document rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtectedPlaceholder {
    Locked,
    Decrypting,
}

impl ProtectedPlaceholder {
    fn title(self) -> String {
        match self {
            Self::Locked => tr!(NoteLocked),
            Self::Decrypting => tr!(Decrypting),
        }
    }

    fn hint(self) -> String {
        match self {
            Self::Locked => tr!(UnlockHint),
            Self::Decrypting => tr!(DecryptingHint),
        }
    }
}

fn protected_placeholder(model: &AppModel) -> Option<ProtectedPlaceholder> {
    if matches!(
        model.secure_ui_operation.as_ref(),
        Some(SecureUiOperation::OpenProtected)
    ) {
        return Some(ProtectedPlaceholder::Decrypting);
    }
    if model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .is_some()
    {
        return None;
    }
    let protected_selected = model.workspace.as_ref().is_some_and(|workspace| {
        workspace.selected_note().is_some_and(|index| {
            workspace
                .notes()
                .get(index)
                .is_some_and(|note| note.protection == NoteProtection::Protected)
        })
    });
    protected_selected.then_some(ProtectedPlaceholder::Locked)
}

fn decrypt_lock_frame(frame: u64) -> &'static str {
    ICON_DECRYPT_FRAMES[(frame % ICON_DECRYPT_FRAMES.len() as u64) as usize]
}

fn schedule_decrypt_frame(
    model: Rc<RefCell<AppModel>>,
    frame: RwSignal<u64>,
    running: RwSignal<bool>,
) {
    exec_after(Duration::from_millis(DECRYPT_FRAME_MS), move |_| {
        let Some(current) = frame.try_get_untracked() else {
            return;
        };
        if running.try_get_untracked().is_none() {
            return;
        }
        // The clock only runs while the card is on screen; the effect below
        // starts it again on the next decryption.
        if !matches!(
            protected_placeholder(&model.borrow()),
            Some(ProtectedPlaceholder::Decrypting)
        ) {
            running.set(false);
            return;
        }
        frame.set(current.wrapping_add(1));
        schedule_decrypt_frame(model, frame, running);
    });
}

fn protected_placeholder_card(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    palette: Palette,
) -> impl IntoView {
    let frame = create_rw_signal(0_u64);
    let running = create_rw_signal(false);
    let clock_model = model.clone();
    create_effect(move |_| {
        revision.get();
        let decrypting = matches!(
            protected_placeholder(&clock_model.borrow()),
            Some(ProtectedPlaceholder::Decrypting)
        );
        if decrypting && !running.get_untracked() {
            running.set(true);
            schedule_decrypt_frame(clock_model.clone(), frame, running);
        }
    });

    let badge_model = model.clone();
    let title_model = model.clone();
    let hint_model = model.clone();
    let visibility_model = model;

    let badge = dyn_container(
        move || {
            revision.get();
            match protected_placeholder(&badge_model.borrow()) {
                Some(ProtectedPlaceholder::Decrypting) => decrypt_lock_frame(frame.get()),
                _ => ICON_LOCK,
            }
        },
        move |icon| {
            svg(icon)
                .style(move |style| style.size(34.0, 34.0).color(palette.accent))
                .into_any()
        },
    )
    .style(move |style| {
        style
            .size(74.0, 74.0)
            .items_center()
            .justify_center()
            .background(palette.accent_soft)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(37.0)
    });

    let card = v_stack((
        badge,
        empty().style(|style| style.height(18.0)),
        caption(move || tr!(ProtectedNote), palette.ink2),
        empty().style(|style| style.height(7.0)),
        label(move || {
            revision.get();
            protected_placeholder(&title_model.borrow())
                .map(ProtectedPlaceholder::title)
                .unwrap_or_default()
                .to_owned()
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_SECTION as f32)
                .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                .font_weight(floem::text::Weight::SEMIBOLD)
                .color(palette.ink)
                .selectable(false)
        }),
        empty().style(|style| style.height(8.0)),
        label(move || {
            revision.get();
            protected_placeholder(&hint_model.borrow())
                .map(ProtectedPlaceholder::hint)
                .unwrap_or_default()
                .to_owned()
        })
        .style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
                .selectable(false)
        }),
    ))
    .style(move |style| {
        style
            .items_center()
            .padding_vert(30.0)
            .padding_horiz(34.0)
            .background(palette.paper)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(14.0)
            .font_family(UI_FONT_FAMILY.to_owned())
    });

    // Nothing here is clickable: the card must not swallow pointer input that
    // belongs to the editor surface underneath. It also keeps the default
    // z-index: floem compares z-index across the whole window, so a raised
    // card inside the editor would paint over the password modal at the root
    // and leave a dialog that takes every click while staying invisible.
    container(card)
        .pointer_events(|| false)
        .style(move |style| {
            revision.get();
            let style = style
                .absolute()
                .size_full()
                .items_center()
                .justify_center()
                .background(palette.paper);
            if protected_placeholder(&visibility_model.borrow()).is_some() {
                style
            } else {
                style.hide()
            }
        })
}

fn render_editor(model: &AppModel) -> String {
    // Locked and decrypting notes are drawn by the placeholder card overlay,
    // so the text surface stays empty instead of duplicating its wording.
    if protected_placeholder(model).is_some() {
        return String::new();
    }
    if model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .is_none()
    {
        return tr!(OpenNoteHint).to_owned();
    }
    let Some(layout) = editor_layout(model) else {
        return tr!(ViewportFailed);
    };
    let mut rendered = String::with_capacity(layout.snapshot.rendered_bytes.min(300_000));
    for (row_index, row) in layout.geometry.rows().iter().enumerate() {
        rendered.push_str(layout.row_text(row_index).unwrap_or_default());
        if row.last_in_line && layout.snapshot.lines[row.line_slot].truncated {
            rendered.push_str("  …");
        }
        rendered.push('\n');
    }
    rendered
}

fn render_editor_line_numbers(model: &AppModel) -> String {
    let Some(layout) = editor_layout(model) else {
        return String::new();
    };
    let digits = decimal_digits(layout.snapshot.total_lines.max(1));
    let mut rendered = String::with_capacity((layout.geometry.rows().len() + 2) * (digits + 1));
    for row in layout.geometry.rows() {
        if row.layout_row == 0 {
            rendered.push_str(&format!("{:>digits$}", row.line_index + 1));
        } else {
            rendered.push_str(&" ".repeat(digits));
        }
        rendered.push('\n');
    }
    rendered
}

fn editor_status(model: &AppModel) -> String {
    if let Some(error) = &model.error {
        return tr!(ErrorStatus , "error" => i18n::user_error_text(error));
    }
    if model.secure_worker_active {
        return tr!(SecureRunning);
    }
    let Some(document) = model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
    else {
        if model.workspace.as_ref().is_some_and(|workspace| {
            workspace.selected_note().is_some_and(|index| {
                workspace
                    .notes()
                    .get(index)
                    .is_some_and(|note| note.protection == NoteProtection::Protected)
            })
        }) {
            return tr!(NoteLocked);
        }
        return tr!(NoOpenNote);
    };
    let line = document.cursor_line().unwrap_or(0) + 1;
    let column = document.cursor_byte_column().unwrap_or(0) + 1;
    let selection = document.selection_character_count();
    let save = match document.save_status() {
        SaveStatus::Clean { .. } => tr!(Saved),
        SaveStatus::Dirty { .. } => tr!(Modified),
        SaveStatus::Saving {
            dirty_after_start: false,
            ..
        } => tr!(Saving),
        SaveStatus::Saving {
            dirty_after_start: true,
            ..
        } => tr!(SavingMore),
        SaveStatus::Error { message, .. } => {
            tr!(SaveError , "value" => localize_storage_message(&message))
        }
        SaveStatus::Conflict { message, .. } => {
            tr!(Conflict , "value" => localize_storage_message(&message))
        }
    };
    let recovery = match document.recovery_status() {
        RecoveryStatus::None => String::new(),
        RecoveryStatus::Pending { .. }
        | RecoveryStatus::Saving { .. }
        | RecoveryStatus::Saved { .. } => String::new(),
        RecoveryStatus::Error { .. } => tr!(RecoveryError),
    };
    let selection = if selection == 0 {
        String::new()
    } else {
        tr!(SelectionSize , "value" => selection)
    };
    tr!(EditorStatus , "line" => line, "column" => column, "selection" => selection, "value" => format_byte_count(document.len_bytes()), "save" => save, "recovery" => recovery)
}

fn localize_storage_message(message: &str) -> String {
    match message {
        "note changed on disk while local edits were pending; both versions are preserved" => {
            tr!(DiskConflict)
        }
        other => i18n::user_error_text(&UiText::from(other)),
    }
}

fn format_byte_count(bytes: usize) -> String {
    if bytes >= 1_000_000 {
        tr!(Megabytes , "value" => format!("{:.1}", bytes as f64 / 1_000_000.0))
    } else if bytes >= 1_000 {
        tr!(Kilobytes , "value" => format!("{:.1}", bytes as f64 / 1_000.0))
    } else {
        tr!(Bytes , "bytes" => bytes)
    }
}

fn selected_note_tags(model: &AppModel) -> Vec<String> {
    model
        .workspace
        .as_ref()
        .and_then(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
                .map(|note| note.tags.clone())
        })
        .unwrap_or_default()
}

fn model_tag_suggestions(model: &AppModel, query: &str) -> Vec<String> {
    let Some(workspace) = model.workspace.as_ref() else {
        return Vec::new();
    };
    let assigned = workspace
        .selected_note()
        .and_then(|index| workspace.notes().get(index))
        .map(|note| note.tags.as_slice())
        .unwrap_or_default();
    tag_suggestions(
        workspace
            .categories()
            .iter()
            .map(|category| category.name.as_str()),
        assigned,
        query,
    )
}

fn add_tag_from_popover(
    model: &Rc<RefCell<AppModel>>,
    signals: TagPopoverSignals,
    revision: RwSignal<u64>,
    input_id: ViewId,
) {
    let query = signals.query.get_untracked();
    let suggestions = model_tag_suggestions(&model.borrow(), &query);
    let Some(tag) = tag_submission(&query, &suggestions, signals.highlighted.get_untracked())
    else {
        return;
    };
    add_tag_value_from_popover(model, signals, revision, input_id, &tag);
}

fn add_tag_value_from_popover(
    model: &Rc<RefCell<AppModel>>,
    signals: TagPopoverSignals,
    revision: RwSignal<u64>,
    input_id: ViewId,
    tag: &str,
) {
    let added = model.borrow_mut().add_tag_selected(tag);
    revision.update(|value| *value += 1);
    if !added {
        return;
    }
    signals.query.set(String::new());
    signals.highlighted.set(None);
    signals.hovered_tag.set(None);
    schedule_autosave(model.clone(), revision);
    input_id.request_focus();
}

fn tag_popover_card(
    model: Rc<RefCell<AppModel>>,
    revision: RwSignal<u64>,
    sidebar_state: RwSignal<SidebarState>,
    signals: TagPopoverSignals,
    tag_button_id: ViewId,
    palette: Palette,
) -> impl IntoView {
    let input = localized_input::LocalizedInput::new(signals.query, i18n::Key::AddTag).style(
        move |style| {
            text_input_affordance(style, palette.muted, palette.accent)
                .width_full()
                .height(TAG_POPOVER_ROW_HEIGHT_PX)
                .items_center()
                .padding_horiz(10.0)
                .background(palette.canvas)
                .color(palette.ink)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(5.0)
                .font_size(crate::ui::FONT_BODY as f32)
        },
    );
    let input_id = input.id();
    let input_key_model = model.clone();
    let input = input.on_event(EventListener::KeyDown, move |event| {
        let Event::KeyDown(key_event) = event else {
            return EventPropagation::Continue;
        };
        match &key_event.key.logical_key {
            Key::Named(NamedKey::Escape) => {
                close_tag_popover(signals);
                tag_button_id.request_focus();
                EventPropagation::Stop
            }
            Key::Named(NamedKey::ArrowDown) => {
                let suggestions = model_tag_suggestions(
                    &input_key_model.borrow(),
                    &signals.query.get_untracked(),
                );
                signals.highlighted.set(move_tag_suggestion_highlight(
                    signals.highlighted.get_untracked(),
                    suggestions.len(),
                    TagSuggestionDirection::Next,
                ));
                EventPropagation::Stop
            }
            Key::Named(NamedKey::ArrowUp) => {
                let suggestions = model_tag_suggestions(
                    &input_key_model.borrow(),
                    &signals.query.get_untracked(),
                );
                signals.highlighted.set(move_tag_suggestion_highlight(
                    signals.highlighted.get_untracked(),
                    suggestions.len(),
                    TagSuggestionDirection::Previous,
                ));
                EventPropagation::Stop
            }
            Key::Named(NamedKey::Enter) => {
                add_tag_from_popover(&input_key_model, signals, revision, input_id);
                EventPropagation::Stop
            }
            _ => EventPropagation::Continue,
        }
    });
    let reset_highlight = signals;
    create_effect(move |_| {
        reset_highlight.query.get();
        reset_highlight.highlighted.set(None);
    });

    let empty_state_model = model.clone();
    let empty_state = label(move || tr!(NoTags)).style(move |style| {
        revision.get();
        let style = style
            .height(TAG_POPOVER_ROW_HEIGHT_PX)
            .width_full()
            .items_center()
            .padding_horiz(10.0)
            .font_size(crate::ui::FONT_BODY as f32)
            .color(palette.muted);
        if selected_note_tags(&empty_state_model.borrow()).is_empty() {
            style
        } else {
            style.hide()
        }
    });

    let assigned_model = model.clone();
    let assigned_row_model = model.clone();
    let assigned_rows = dyn_stack(
        move || {
            revision.get();
            selected_note_tags(&assigned_model.borrow())
        },
        |tag| tag.clone(),
        move |tag| {
            let hover_tag = tag.clone();
            let leave_tag = tag.clone();
            let label_tag = tag.clone();
            let remove_tag_value = tag.clone();
            let tooltip_tag = tag.clone();
            let remove_model = assigned_row_model.clone();
            let remove_button = compact_icon_button(
                || ICON_CANCEL,
                move || tr!(RemoveTag, "tag" => tooltip_tag.clone()),
                IconButtonTone::Secondary,
                palette,
                20.0,
                || true,
                move || {
                    let removed =
                        remove_tag(&remove_model, sidebar_state, &remove_tag_value, revision);
                    revision.update(|value| *value += 1);
                    if removed {
                        signals.query.set(String::new());
                        signals.highlighted.set(None);
                        signals.hovered_tag.set(None);
                        input_id.request_focus();
                    }
                },
            )
            .style(move |style| {
                let visible = signals.hovered_tag.get().as_deref() == Some(tag.as_str());
                style
                    .size(24.0, 24.0)
                    .items_center()
                    .justify_center()
                    .border_radius(4.0)
                    .color(if visible {
                        palette.muted
                    } else {
                        Color::TRANSPARENT
                    })
                    .hover(move |style| {
                        style
                            .background(Color::rgb8(250, 235, 235))
                            .color(palette.danger)
                    })
                    .focus_visible(move |style| style.color(palette.muted))
            });
            h_stack((
                label(move || label_tag.clone()).style(move |style| {
                    style
                        .min_width(0.0)
                        .flex_grow(1.0)
                        .font_size(crate::ui::FONT_BODY as f32)
                        .color(palette.ink)
                        .text_ellipsis()
                }),
                remove_button,
            ))
            .on_event(EventListener::PointerMove, move |_| {
                if signals.hovered_tag.get_untracked().as_deref() != Some(hover_tag.as_str()) {
                    signals.hovered_tag.set(Some(hover_tag.clone()));
                }
                EventPropagation::Continue
            })
            .on_event(EventListener::PointerLeave, move |_| {
                if signals.hovered_tag.get_untracked().as_deref() == Some(leave_tag.as_str()) {
                    signals.hovered_tag.set(None);
                }
                EventPropagation::Continue
            })
            .style(move |style| {
                style
                    .height(TAG_POPOVER_ROW_HEIGHT_PX)
                    .width_full()
                    .items_center()
                    .gap(6.0)
                    .padding_left(10.0)
                    .padding_right(5.0)
                    .border_radius(5.0)
                    .hover(move |style| style.background(palette.accent_soft))
            })
        },
    )
    .style(|style| style.width_full().flex_col().gap(TAG_POPOVER_ROW_GAP_PX));

    let suggestion_state_model = model.clone();
    let suggestion_row_model = model.clone();
    let suggestion_rows = dyn_stack(
        move || {
            revision.get();
            model_tag_suggestions(&suggestion_state_model.borrow(), &signals.query.get())
                .into_iter()
                .enumerate()
                .collect::<Vec<_>>()
        },
        |(_, tag)| tag.clone(),
        move |(index, tag)| {
            let action_tag = tag.clone();
            let row_model = suggestion_row_model.clone();
            selectable_row(
                label(move || tag.clone()).style(move |style| {
                    style
                        .min_width(0.0)
                        .width_full()
                        .font_size(crate::ui::FONT_BODY as f32)
                        .color(palette.ink)
                        .text_ellipsis()
                }),
                move || {
                    add_tag_value_from_popover(
                        &row_model,
                        signals,
                        revision,
                        input_id,
                        &action_tag,
                    );
                },
            )
            .style(move |style| {
                let selected = signals.highlighted.get() == Some(index);
                style
                    .height(TAG_POPOVER_ROW_HEIGHT_PX)
                    .width_full()
                    .items_center()
                    .padding_horiz(10.0)
                    .border_radius(5.0)
                    .background(if selected {
                        palette.accent_soft
                    } else {
                        palette.paper
                    })
                    .hover(move |style| style.background(palette.accent_soft))
            })
        },
    )
    .style(move |style| {
        let style = style
            .width_full()
            .flex_col()
            .gap(TAG_POPOVER_ROW_GAP_PX)
            .margin_top(TAG_POPOVER_SECTION_GAP_PX - TAG_POPOVER_ROW_GAP_PX)
            .padding_top(TAG_POPOVER_SECTION_GAP_PX)
            .border_top(1.0)
            .border_color(palette.divider);
        if signals.query.get().trim().is_empty() {
            style.hide()
        } else {
            style
        }
    });

    let list = scroll(
        v_stack((empty_state, assigned_rows, suggestion_rows)).style(|style| {
            style
                .width_full()
                .gap(TAG_POPOVER_ROW_GAP_PX)
                .padding_right(TAG_POPOVER_GUTTER_PX)
        }),
    )
    .scroll_style(move |style| {
        style
            .handle_thickness(TAG_POPOVER_SCROLLBAR_PX)
            .handle_rounded(true)
            .handle_background(palette.scrollbar)
    })
    .style(|style| {
        style
            .width(TAG_POPOVER_CONTENT_WIDTH_PX + TAG_POPOVER_GUTTER_PX)
            .max_height(TAG_POPOVER_LIST_MAX_HEIGHT_PX)
    });
    let card = v_stack((
        list,
        container(input).style(move |style| {
            style
                .width(TAG_POPOVER_CONTENT_WIDTH_PX)
                .padding_top(TAG_POPOVER_SECTION_GAP_PX)
                .border_top(1.0)
                .border_color(palette.divider)
        }),
    ))
    .style(move |style| {
        style
            .width(TAG_POPOVER_WIDTH_PX)
            .gap(TAG_POPOVER_SECTION_GAP_PX)
            .padding(TAG_POPOVER_PADDING_PX)
            .padding_right(TAG_POPOVER_SCROLLBAR_INSET_PX)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(7.0)
    });
    exec_after(Duration::from_millis(10), move |_| {
        if signals.open.try_get_untracked() == Some(true) && input_id.parent().is_some() {
            input_id.request_focus();
        }
    });
    card
}

fn remove_tag(
    model: &Rc<RefCell<AppModel>>,
    sidebar_state: RwSignal<SidebarState>,
    tag: &str,
    revision: RwSignal<u64>,
) -> bool {
    let removed = model.borrow_mut().remove_tag_selected(tag);
    if !removed {
        return false;
    }
    schedule_autosave(model.clone(), revision);
    let normalized = tag.trim();
    let still_exists = model.borrow().workspace.as_ref().is_some_and(|workspace| {
        sidebar_category_paths(
            workspace
                .categories()
                .iter()
                .map(|category| category.name.as_str()),
        )
        .contains(normalized)
    });
    if !still_exists {
        let normalized = normalized.to_owned();
        sidebar_state.update(|state| {
            state
                .expanded
                .remove(&SidebarFilter::Tag(normalized.clone()));
            if matches!(
                &state.creation_group,
                SidebarFilter::Tag(selected) if selected == &normalized
            ) {
                state.creation_group = SidebarFilter::All;
            }
        });
    }
    true
}

fn selected_note_flag(
    model: &Rc<RefCell<AppModel>>,
    predicate: impl FnOnce(&stillus_core::NoteSummary) -> bool,
) -> bool {
    let model = model.borrow();
    model
        .workspace
        .as_ref()
        .and_then(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
        })
        .is_some_and(predicate)
}

/// The item a toolbar acts on. Engines share one control per action and name
/// it with their own noun, so the shared button takes the subject instead of
/// a ready-made tooltip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolbarSubject {
    Note,
    Feed,
    Chat,
}

fn toolbar_action_icon(action: ToolbarAction) -> &'static str {
    match action {
        ToolbarAction::Filters => ButtonAction::Settings.icon(),
        ToolbarAction::Refresh => ButtonAction::Refresh.icon(),
        ToolbarAction::Rename => ButtonAction::Edit.icon(),
        ToolbarAction::Categories => ICON_TAG,
        ToolbarAction::Pin => ButtonAction::Pin.icon(),
        ToolbarAction::Favorite => ButtonAction::Favorite.icon(),
        ToolbarAction::Delete => ButtonAction::Delete.icon(),
        ToolbarAction::Restore => ICON_RECOVER,
    }
}

fn toolbar_action_tone(action: ToolbarAction) -> IconButtonTone {
    match action {
        ToolbarAction::Delete => IconButtonTone::Danger,
        _ => IconButtonTone::Secondary,
    }
}

/// Controls that stay lit while their state is on: a pinned or favorited
/// item, a running refresh and an open editing bar all show it in place.
/// Delete and restore never light up, because each already replaces the other.
fn toolbar_action_is_toggle(action: ToolbarAction) -> bool {
    !matches!(action, ToolbarAction::Delete | ToolbarAction::Restore)
}

fn toolbar_action_title(action: ToolbarAction, subject: ToolbarSubject, active: bool) -> String {
    match action {
        ToolbarAction::Filters => tr!(RssFilters),
        ToolbarAction::Refresh => {
            if active {
                tr!(Refreshing)
            } else {
                match subject {
                    ToolbarSubject::Note => tr!(RefreshNote),
                    ToolbarSubject::Feed => tr!(RefreshFeed),
                    ToolbarSubject::Chat => tr!(ChatRunning),
                }
            }
        }
        ToolbarAction::Rename => {
            if active {
                tr!(CloseRename)
            } else {
                match subject {
                    ToolbarSubject::Note => tr!(RenameNote),
                    ToolbarSubject::Feed => tr!(RenameFeed),
                    ToolbarSubject::Chat => tr!(ChatRename),
                }
            }
        }
        ToolbarAction::Categories => match subject {
            ToolbarSubject::Note => tr!(ManageTags),
            ToolbarSubject::Chat => tr!(ChatCategories),
            ToolbarSubject::Feed => {
                if active {
                    tr!(CloseCategories)
                } else {
                    tr!(EditFeedCategories)
                }
            }
        },
        ToolbarAction::Pin => {
            if active {
                match subject {
                    ToolbarSubject::Note => tr!(UnpinNote),
                    ToolbarSubject::Feed => tr!(UnpinFeed),
                    ToolbarSubject::Chat => tr!(ChatUnpin),
                }
            } else {
                match subject {
                    ToolbarSubject::Note => tr!(PinNote),
                    ToolbarSubject::Feed => tr!(PinFeed),
                    ToolbarSubject::Chat => tr!(ChatPin),
                }
            }
        }
        ToolbarAction::Favorite => {
            if active {
                tr!(RemoveFavorite)
            } else {
                tr!(AddFavorite)
            }
        }
        ToolbarAction::Delete => match subject {
            ToolbarSubject::Note => tr!(TrashNote),
            ToolbarSubject::Feed => tr!(TrashFeed),
            ToolbarSubject::Chat => tr!(ChatTrash),
        },
        ToolbarAction::Restore => match subject {
            ToolbarSubject::Note => tr!(RestoreNote),
            ToolbarSubject::Feed => tr!(RestoreFeed),
            ToolbarSubject::Chat => tr!(ChatRestore),
        },
    }
}

/// Delete and restore share one slot: an engine declares both and the toolbar
/// shows the one that matches the current item state.
fn visible_toolbar_actions(declared: &[ToolbarAction], deleted: bool) -> Vec<ToolbarAction> {
    declared
        .iter()
        .copied()
        .filter(|action| match action {
            ToolbarAction::Delete => !deleted,
            ToolbarAction::Restore => deleted,
            _ => true,
        })
        .collect()
}

/// The control every engine surface renders for a declared toolbar action.
fn toolbar_control(
    action: ToolbarAction,
    subject: ToolbarSubject,
    palette: Palette,
    active: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
) -> AnyView {
    enabled_toolbar_control(action, subject, palette, || true, active, on_press)
}

fn busy_note_toolbar_control(
    action: ToolbarAction,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    active: impl Fn() -> bool + 'static,
    busy: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
) -> AnyView {
    let active: Rc<dyn Fn() -> bool> = Rc::new(active);
    let title_active = active.clone();
    busy_icon_toggle_button(
        toolbar_action_icon(action),
        move || toolbar_action_title(action, ToolbarSubject::Note, title_active()),
        palette,
        enabled,
        move || active(),
        busy,
        on_press,
    )
}

fn enabled_toolbar_control(
    action: ToolbarAction,
    subject: ToolbarSubject,
    palette: Palette,
    enabled: impl Fn() -> bool + 'static,
    active: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
) -> AnyView {
    let icon = toolbar_action_icon(action);
    if toolbar_action_is_toggle(action) {
        let active: Rc<dyn Fn() -> bool> = Rc::new(active);
        let title_active = active.clone();
        enabled_icon_toggle_button(
            icon,
            move || toolbar_action_title(action, subject, title_active()),
            palette,
            enabled,
            move || active(),
            on_press,
        )
    } else {
        enabled_icon_button(
            icon,
            move || toolbar_action_title(action, subject, false),
            toolbar_action_tone(action),
            palette,
            enabled,
            on_press,
        )
    }
}

/// Comma-separated categories as an engine stores them: trimmed, without
/// blanks and without repeats, in the order they were typed.
fn parsed_category_list(input: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    input
        .split(',')
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .filter(|category| seen.insert((*category).to_owned()))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::WorkspaceSession;
    use super::test_support::{Deadline, workspace as test_workspace};
    use super::{
        AppModel, CategoryDropPosition, EDITOR_CHARACTER_WIDTH_PX, EDITOR_LINE_HEIGHT_PX,
        EDITOR_LINE_NUMBER_GAP_PX, EDITOR_LINE_NUMBER_MIN_WIDTH_PX, EDITOR_PADDING_X_PX,
        EDITOR_PADDING_Y_PX, EditorMenuState, FAVORITED_ORDER_KEY, GoToLineError, LaunchError,
        LaunchOptions, MAX_PASSWORD_BYTES, MasterPassword, NoteSort, PasswordDialogKind,
        PasswordEntry, PasswordFeedback, PasswordField, PasswordSubmitOutcome,
        PendingPasswordChange, PendingPasswordChangeState, PendingSecurityAction,
        SIDEBAR_MAX_WIDTH_PX, SIDEBAR_MIN_WIDTH_PX, SearchCommand, SearchEvent, SecurePhase,
        SecureProgress, SecureUiOperation, SecurityUi, SidebarFilter, SidebarGroupToggle,
        SidebarRow, SidebarState, StartupCandidateState, StartupWorkspace, TagSuggestionDirection,
        UnlockOutcome, WorkspaceSwitchBlocker, caret_geometry, category_drag_hit_regions,
        category_drag_source_at_point, category_drag_threshold_reached,
        category_drop_target_at_point, category_path_segments, current_sidebar_rows,
        decimal_digits, editor_command_for_point, editor_command_for_pointer,
        editor_drag_command_for_point, editor_horizontal_metrics, editor_layout,
        editor_line_command_for_point, editor_line_number_width, editor_menu_state,
        editor_selection_is_fully_visible, editor_selection_rects, editor_wheel_line_delta,
        editor_word_command_for_point, external_file_picker_spec, go_to_line,
        is_current_search_generation, is_primary_pointer_down, is_toggle_task_done_shortcut,
        matching_tag_indices, move_tag_suggestion_highlight, note_drop_target, note_matches_filter,
        parse_go_to_line, password_change_busy, password_change_progress_text,
        password_change_success_text, prepare_workspace_switch, probe_editor_font,
        protection_action_state, protection_password_dialog, recovery_password_outcome,
        render_editor, render_editor_line_numbers, reordered_catalog_items, resized_sidebar_width,
        resolve_startup_workspace, search_worker, sidebar_category_tree,
        sidebar_note_indicator_icons, sidebar_rows, sidebar_tree_indent, startup_candidate_state,
        tag_submission, tag_suggestions, toolbar_action_icon, toolbar_action_is_toggle,
        toolbar_action_title, toolbar_action_tone, visible_toolbar_actions,
        workspace_switch_blocker,
    };
    use super::{IconButtonTone, ToolbarSubject, parsed_category_list};
    use crate::i18n::{Key, msg, tr};
    use crate::settings::{
        CategoryNoteSortSettings, GlobalSettings, NoteSortField, PersistedExternalFile,
        PersistedSidebarGroup, SidebarSettings, SortDirection,
    };
    use floem::event::Event;
    use floem::keyboard::{KeyCode, Modifiers, PhysicalKey};
    use floem::kurbo::Point;
    use floem::pointer::{PointerButton, PointerInputEvent};
    use floem::prelude::{SignalGet, SignalUpdate};
    use std::fs;
    use std::time::Duration;
    use stillus_core::{
        CatalogOrderItem, DocumentTarget, EditorCommand, RssEntry, RssFeedCache, RssRefreshResult,
        SecureWorkerEvent, ToolbarAction,
    };

    #[cfg(feature = "test-utils")]
    use super::SecurityActionOutcome;

    fn pointer_down(button: PointerButton) -> Event {
        Event::PointerDown(PointerInputEvent {
            pos: (12.0, 34.0).into(),
            button,
            modifiers: Default::default(),
            count: 1,
        })
    }

    #[test]
    fn reliable_activation_accepts_only_primary_pointer_down() {
        assert!(is_primary_pointer_down(&pointer_down(
            PointerButton::Primary
        )));
        assert!(!is_primary_pointer_down(&pointer_down(
            PointerButton::Secondary
        )));
        assert!(!is_primary_pointer_down(&Event::FocusGained));
    }

    #[test]
    fn language_switch_preserves_dirty_editor_selection_undo_and_files() {
        let root = test_workspace("stillus-locale-state");
        fs::create_dir_all(root.join("notes")).unwrap();
        let path = root.join("notes/Existing.md");
        let original = "# Existing\nKeep this text unchanged on disk.\n";
        fs::write(&path, original).unwrap();
        let mut model = AppModel::load(&root);
        let original_editor = render_editor(&model);
        model.apply(EditorCommand::Insert("draft ".to_owned()));
        model.apply(EditorCommand::SetSelection {
            anchor: 1,
            focus: 4,
        });
        model.error = Some(msg!(NoSelection).into());
        let document = model.workspace.as_ref().unwrap().document().unwrap();
        let selection = document.selection();
        let status = document.save_status();
        let visible = render_editor(&model);
        let scroll = (model.viewport_first_line, model.viewport_first_visual_row);
        for locale in crate::i18n::Locale::ALL {
            crate::i18n::set_current(*locale);
            let document = model.workspace.as_ref().unwrap().document().unwrap();
            assert_eq!(document.selection(), selection);
            assert_eq!(document.save_status(), status);
            assert_eq!(render_editor(&model), visible);
            assert_eq!(
                (model.viewport_first_line, model.viewport_first_visual_row),
                scroll
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            assert_eq!(model.error.as_ref().unwrap().to_string(), tr!(NoSelection));
            assert!(!root.join(".stillus.cfg").exists());
        }
        crate::i18n::set_current(crate::i18n::Locale::English);
        model.apply(EditorCommand::Undo);
        assert_eq!(render_editor(&model), original_editor);
        model.shutdown_search_worker();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_file_picker_uses_registered_extensions() {
        let root = test_workspace("stillus-app-file-picker");
        fs::create_dir_all(root.join("notes")).expect("create picker workspace");
        let workspace = WorkspaceSession::open(&root).unwrap();
        let spec = external_file_picker_spec(workspace.external_file_extensions()).unwrap();

        assert_eq!(spec.name, tr!(SupportedFiles));
        assert_eq!(spec.extensions, ["markdown", "md", "txt"]);
        drop(workspace);
        fs::remove_dir_all(root).expect("remove picker workspace");
    }

    #[test]
    fn protection_action_icons_distinguish_locked_and_unlocked_notes() {
        assert_eq!(
            super::ProtectionActionState::Lock.icon(),
            Some(super::ICON_LOCK)
        );
        assert_eq!(
            super::ProtectionActionState::Decrypting.icon(),
            Some(super::ICON_LOCK)
        );
        assert_eq!(
            super::ProtectionActionState::Unlock { note_index: 3 }.icon(),
            Some(super::ICON_UNLOCK)
        );
        assert_eq!(
            super::ProtectionActionState::UnlockKnown { note_index: 3 }.icon(),
            Some(super::ICON_UNLOCK)
        );
    }

    #[test]
    fn decrypt_badge_cycles_through_every_lock_frame() {
        let frames = super::ICON_DECRYPT_FRAMES;
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(super::decrypt_lock_frame(index as u64), *frame);
        }
        assert_eq!(super::decrypt_lock_frame(frames.len() as u64), frames[0]);
        assert_ne!(super::decrypt_lock_frame(1), super::decrypt_lock_frame(0));
    }

    #[test]
    fn toggle_task_done_requires_only_alt_and_physical_key_d() {
        let key_d = PhysicalKey::Code(KeyCode::KeyD);
        assert!(is_toggle_task_done_shortcut(Modifiers::ALT, key_d));
        assert!(!is_toggle_task_done_shortcut(Modifiers::empty(), key_d));
        assert!(!is_toggle_task_done_shortcut(
            Modifiers::ALT | Modifiers::SHIFT,
            key_d,
        ));
        assert!(!is_toggle_task_done_shortcut(
            Modifiers::ALT | Modifiers::CONTROL,
            key_d,
        ));
        assert!(!is_toggle_task_done_shortcut(
            Modifiers::ALT | Modifiers::META,
            key_d,
        ));
        assert!(!is_toggle_task_done_shortcut(
            Modifiers::ALT,
            PhysicalKey::Code(KeyCode::KeyE),
        ));

        let command = if is_toggle_task_done_shortcut(Modifiers::ALT, key_d) {
            EditorCommand::ToggleTaskDone
        } else {
            EditorCommand::Insert("∂".to_owned())
        };
        assert_eq!(command, EditorCommand::ToggleTaskDone);
    }

    #[test]
    fn sidebar_display_width_preserves_user_width_and_limits_narrow_windows() {
        assert_eq!(super::displayed_sidebar_width(480.0, 960.0, false), 200.0);
        assert_eq!(super::displayed_sidebar_width(480.0, 1100.0, false), 440.0);
        assert_eq!(super::displayed_sidebar_width(256.0, 1240.0, false), 256.0);
        assert_eq!(super::displayed_sidebar_width(480.0, 960.0, true), 56.0);
        assert_eq!(super::displayed_sidebar_width(420.0, 1240.0, false), 420.0);
    }

    #[test]
    fn sidebar_resize_delta_is_live_and_clamped() {
        assert_eq!(resized_sidebar_width(256.0, 84.0, 4.0), 336.0);
        assert_eq!(
            resized_sidebar_width(336.0, -200.0, 4.0),
            SIDEBAR_MIN_WIDTH_PX
        );
        assert_eq!(
            resized_sidebar_width(180.0, 900.0, 4.0),
            SIDEBAR_MAX_WIDTH_PX
        );
        assert_eq!(resized_sidebar_width(480.0, -36.0, 4.0), 440.0);
    }

    #[test]
    fn popovers_mirror_their_anchor_and_stay_inside_the_window() {
        for rtl in [false, true] {
            for start in [false, true] {
                for origin in [8.0, 190.0, 670.0, 820.0] {
                    let left = super::popover_left(origin, 32.0, 280.0, 860.0, start, rtl);
                    assert!(left >= 8.0 && left + 280.0 <= 852.0);
                }
            }
        }
        assert_eq!(
            super::popover_left(300.0, 32.0, 200.0, 860.0, true, false),
            300.0
        );
        assert_eq!(
            super::popover_left(300.0, 32.0, 200.0, 860.0, true, true),
            132.0
        );
    }

    #[test]
    fn launch_accepts_file_batches_and_explicit_workspace() {
        assert_eq!(
            LaunchOptions::parse_from(["--open", "--", "--literal.txt"])
                .unwrap()
                .external_paths,
            [std::path::PathBuf::from("--literal.txt")]
        );
        let parsed = LaunchOptions::parse_from([
            "--workspace",
            "workspace",
            "--open",
            "one.MD",
            "Заметка #2.markdown",
            "--smoke-exit-ms",
            "25",
            "--",
            "-third.txt",
        ])
        .unwrap();
        assert_eq!(parsed.workspace, Some("workspace".into()));
        assert_eq!(
            parsed.external_paths,
            ["one.MD", "Заметка #2.markdown", "-third.txt"].map(std::path::PathBuf::from)
        );
        assert_eq!(
            LaunchOptions::parse_from(["one.md", "two.txt"])
                .unwrap()
                .external_paths
                .len(),
            2
        );
        for args in [
            vec!["--open"],
            vec!["--workspace"],
            vec!["--open", "--workspace", "workspace"],
        ] {
            assert!(matches!(
                LaunchOptions::parse_from(args),
                Err(LaunchError::MissingValue(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn launch_preserves_non_unicode_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let path = std::ffi::OsString::from_vec(b"note\xff.md".to_vec());
        let parsed = LaunchOptions::parse_from([path.clone()]).unwrap();
        assert_eq!(
            parsed.external_paths[0].as_os_str().as_bytes(),
            path.as_bytes()
        );
    }

    #[test]
    fn desktop_shortcuts_and_altgr_preserve_text_input() {
        use floem::keyboard::{Key, NamedKey};
        assert!(super::altgr_text(
            Modifiers::CONTROL | Modifiers::ALT,
            Some("@")
        ));
        assert!(!super::altgr_text(Modifiers::CONTROL, Some("a")));
        assert!(!super::altgr_text(
            Modifiers::CONTROL | Modifiers::ALT,
            Some("\r")
        ));
        let command = super::platform_edit_command(
            &Key::Named(NamedKey::Home),
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        if cfg!(target_os = "macos") {
            assert!(command.is_none());
        } else {
            assert!(matches!(
                command,
                Some(EditorCommand::MoveDocumentStart { extend: true })
            ));
            assert!(matches!(
                super::platform_edit_command(&Key::Named(NamedKey::End), Modifiers::CONTROL),
                Some(EditorCommand::MoveDocumentEnd { extend: false })
            ));
            assert!(matches!(
                super::platform_edit_command(&Key::Character("y".into()), Modifiers::CONTROL),
                Some(EditorCommand::Redo)
            ));
        }
    }

    #[test]
    fn external_batches_keep_order_duplicates_and_errors() {
        let root = test_workspace("stillus-open-batch");
        fs::create_dir_all(root.join("notes")).unwrap();
        let first = root.join("日本語 one.MD");
        let second = root.join("second.txt");
        fs::write(&first, "First\n").unwrap();
        fs::write(&second, "Second\n").unwrap();
        let mut model = AppModel::load(&root);
        assert!(model.accept_external_paths(&[
            first.clone(),
            root.join("missing.md"),
            second.clone(),
            first.clone()
        ]));
        assert!(model.error.is_some());
        let workspace = model.workspace.as_ref().unwrap();
        assert_eq!(workspace.external_files().len(), 2);
        assert_eq!(fs::read_to_string(&first).unwrap(), "First\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "Second\n");
        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_options_reject_unknown_invalid_and_extra_arguments() {
        let parsed = LaunchOptions::parse_from([
            "--smoke-exit-ms".to_owned(),
            "25".to_owned(),
            "--smoke-autosave".to_owned(),
            "workspace".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            parsed.workspace,
            Some(std::path::PathBuf::from("workspace"))
        );
        assert_eq!(parsed.smoke_exit_after, Some(Duration::from_millis(25)));
        assert!(parsed.smoke_autosave);

        assert_eq!(
            LaunchOptions::parse_from(["--unknown".to_owned()]).unwrap_err(),
            LaunchError::UnknownFlag("--unknown".to_owned())
        );
        assert_eq!(
            LaunchOptions::parse_from(["--smoke-exit-ms".to_owned()]).unwrap_err(),
            LaunchError::MissingValue("--smoke-exit-ms")
        );
        assert_eq!(
            LaunchOptions::parse_from(["--smoke-exit-ms".to_owned(), "later".to_owned()])
                .unwrap_err(),
            LaunchError::InvalidSmokeExit("later".to_owned())
        );
        assert_eq!(
            LaunchOptions::parse_from(["one".to_owned(), "two".to_owned()]).unwrap_err(),
            LaunchError::UnexpectedArgument("two".to_owned())
        );
        assert_eq!(
            LaunchOptions::parse_from(["--".to_owned(), "--workspace".to_owned()])
                .unwrap()
                .workspace,
            Some(std::path::PathBuf::from("--workspace"))
        );
        assert_eq!(
            LaunchOptions::parse_from(Vec::<String>::new())
                .unwrap()
                .workspace,
            None
        );
    }

    #[test]
    fn startup_workspace_prefers_explicit_then_available_global_then_picker() {
        let home = test_workspace("stillus-app-startup-selection");
        let remembered = home.join("remembered");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::create_dir_all(remembered.join("notes")).unwrap();
        let global = GlobalSettings {
            last_workspace: Some(remembered.to_string_lossy().into_owned()),
            ..GlobalSettings::default()
        };

        assert_eq!(
            resolve_startup_workspace(
                Some(std::path::Path::new("explicit")),
                &global,
                Some(&home),
                None,
            ),
            StartupWorkspace::Open(std::path::PathBuf::from("explicit"))
        );
        assert_eq!(
            resolve_startup_workspace(None, &global, Some(&home), None),
            StartupWorkspace::Open(remembered.clone())
        );
        let picker = resolve_startup_workspace(None, &GlobalSettings::default(), Some(&home), None);
        assert!(matches!(
            picker,
            StartupWorkspace::Choose(prompt)
                if prompt.candidate == Some(home.join("Downloads/Notes"))
                    && prompt.diagnostic.is_none()
        ));

        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn unavailable_remembered_workspace_opens_picker_without_recreating_it() {
        let home = test_workspace("stillus-app-stale-startup");
        let missing = home.join("missing");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        let global = GlobalSettings {
            last_workspace: Some(missing.to_string_lossy().into_owned()),
            ..GlobalSettings::default()
        };

        let startup = resolve_startup_workspace(None, &global, Some(&home), None);

        assert!(matches!(
            startup,
            StartupWorkspace::Choose(prompt)
                if prompt.candidate == Some(home.join("Downloads/Notes"))
                    && prompt.diagnostic.is_some()
        ));
        assert!(!missing.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn startup_candidate_requires_confirmation_for_a_new_notes_directory() {
        let root = test_workspace("stillus-app-startup-candidate");
        fs::write(root.join("keep.bin"), b"keep").unwrap();

        assert!(matches!(
            startup_candidate_state(Some(&root), false),
            StartupCandidateState::NeedsInitialization(_)
        ));
        assert!(!root.join("notes").exists());
        assert_eq!(fs::read(root.join("keep.bin")).unwrap(), b"keep");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_without_home_requires_a_manual_folder_choice() {
        let startup = resolve_startup_workspace(
            None,
            &GlobalSettings::default(),
            None,
            Some("HOME is unavailable".to_owned()),
        );

        assert!(matches!(
            startup,
            StartupWorkspace::Choose(prompt)
                if prompt.candidate.is_none()
                    && prompt.diagnostic.as_deref() == Some("HOME is unavailable")
        ));
    }

    #[test]
    fn workspace_switch_preparation_requires_absolute_valid_directory() {
        assert_eq!(
            prepare_workspace_switch(std::path::Path::new("relative/workspace"))
                .err()
                .map(|error| error.to_string()),
            Some(tr!(EnterAbsoluteWorkspace))
        );

        let root = test_workspace("stillus-app-workspace-switch");
        fs::create_dir_all(root.join("notes")).expect("create target workspace");
        fs::write(root.join("notes/Target.md"), "Target\n").expect("write target note");

        let mut prepared = prepare_workspace_switch(&root).expect("prepare valid workspace");
        assert_eq!(prepared.canonical_path, root.canonicalize().unwrap());
        assert!(prepared.model.workspace.is_some());
        prepared.model.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove target workspace");
    }

    #[test]
    fn workspace_switch_blocker_protects_dirty_and_active_operations() {
        let root = test_workspace("stillus-app-workspace-switch-blocker");
        fs::create_dir_all(root.join("notes")).expect("create source workspace");
        fs::write(root.join("notes/Source.md"), "Source\n").expect("write source note");
        let mut model = AppModel::load(&root);

        assert_eq!(workspace_switch_blocker(&model), None);
        assert!(!password_change_busy(&model));
        model.apply(EditorCommand::Insert("dirty ".to_owned()));
        assert_eq!(
            workspace_switch_blocker(&model),
            Some(WorkspaceSwitchBlocker::Unsaved)
        );
        model.save_worker_active = true;
        assert_eq!(
            workspace_switch_blocker(&model),
            Some(WorkspaceSwitchBlocker::Persistence)
        );
        model.save_worker_active = false;
        model.secure_worker_active = true;
        assert_eq!(
            workspace_switch_blocker(&model),
            Some(WorkspaceSwitchBlocker::Security)
        );
        model.secure_worker_active = false;
        model.pending_password_change = Some(PendingPasswordChange {
            current: MasterPassword::new("current".to_owned()),
            new: MasterPassword::new("new".to_owned()),
            state: PendingPasswordChangeState::WaitingPersistence,
        });
        assert_eq!(
            workspace_switch_blocker(&model),
            Some(WorkspaceSwitchBlocker::Security)
        );
        assert!(password_change_busy(&model));
        model.pending_password_change = None;
        model.secure_ui_operation = Some(SecureUiOperation::ChangeMasterPassword);
        assert!(password_change_busy(&model));
        model.secure_ui_operation = None;
        assert!(!password_change_busy(&model));

        model.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove source workspace");
    }

    #[test]
    fn secure_progress_ignores_stale_operations_and_regressions() {
        let root = test_workspace("stillus-app-secure-progress");
        fs::create_dir_all(root.join("notes")).expect("create progress workspace");
        fs::write(root.join("notes/Source.md"), "Source\n").expect("write source note");
        let mut model = AppModel::load(&root);
        model.secure_operation_id = Some(77);

        assert!(model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::Validating,
            completed: 2,
            total: 4,
            percent: Some(0),
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 76,
            phase: SecurePhase::ReplacingNotes,
            completed: 4,
            total: 4,
            percent: Some(80),
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::Validating,
            completed: 1,
            total: 4,
            percent: None,
        }));
        assert!(model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::PreparingNotes,
            completed: 1,
            total: 2,
            percent: Some(25),
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::PreparingNotes,
            completed: 2,
            total: 2,
            percent: Some(24),
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::PreparingNotes,
            completed: 2,
            total: 2,
            percent: None,
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::Validating,
            completed: 4,
            total: 4,
            percent: None,
        }));
        assert!(model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::RollingBack,
            completed: 1,
            total: 2,
            percent: Some(50),
        }));
        assert!(!model.finish_secure_progress(SecureProgress {
            operation_id: 77,
            phase: SecurePhase::RollingBack,
            completed: 2,
            total: 2,
            percent: Some(49),
        }));

        model.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove progress workspace");
    }

    #[test]
    fn password_change_progress_text_adds_only_estimated_percent() {
        let validating = SecureProgress {
            operation_id: 1,
            phase: SecurePhase::Validating,
            completed: 2,
            total: 3,
            percent: Some(0),
        };
        assert_eq!(
            password_change_progress_text("Проверено 2 из 3".to_owned(), validating),
            "0% · Проверено 2 из 3"
        );

        let preparing = SecureProgress {
            operation_id: 1,
            phase: SecurePhase::PreparingNotes,
            completed: 1,
            total: 3,
            percent: Some(42),
        };
        assert_eq!(
            password_change_progress_text("Подготовлено 1 из 3".to_owned(), preparing),
            "42% · Подготовлено 1 из 3"
        );
        assert_eq!(password_change_success_text(3), "100% · Replaced 3 of 3");
    }

    #[test]
    fn password_entry_keeps_both_fields_zeroizing_and_clears_them_together() {
        let mut entry = PasswordEntry::default();
        entry.pop();
        assert!(entry.primary.is_empty());

        assert!(entry.push("пароль🔐"));
        entry.pop();
        assert_eq!(entry.primary.as_str(), "пароль");
        entry.clear();

        assert!(entry.push("correct horse"));
        entry.active = PasswordField::Confirmation;
        assert!(entry.push("correct horse"));
        assert_eq!(entry.primary.len(), entry.confirmation.len());

        let primary = zeroize::Zeroizing::new(entry.take_primary());
        assert_eq!(primary.as_str(), "correct horse");
        assert!(entry.primary.is_empty());
        assert!(entry.primary.capacity() >= MAX_PASSWORD_BYTES);
        entry.clear();
        assert!(entry.confirmation.is_empty());
        assert_eq!(entry.active, PasswordField::Primary);

        assert!(entry.push(&"x".repeat(MAX_PASSWORD_BYTES)));
        assert!(!entry.push("y"));
        assert_eq!(entry.primary.len(), MAX_PASSWORD_BYTES);
    }

    #[test]
    fn pending_security_action_owns_the_zeroizing_master_password_until_consumed() {
        let action = PendingSecurityAction::Protect {
            note_path: "notes/Private.md".into(),
            password: Some(stillus_secure::MasterPassword::new(
                "pending password".to_owned(),
            )),
        };
        let mut pending = Some(action);
        assert!(
            pending
                .as_ref()
                .is_some_and(PendingSecurityAction::has_password)
        );

        let consumed = pending.take().expect("pending action is consumed once");
        assert!(pending.is_none());
        drop(consumed);
    }

    #[test]
    fn recovery_unlock_stays_pending_until_the_background_job_finishes() {
        assert_eq!(
            recovery_password_outcome(UnlockOutcome::Pending),
            PasswordSubmitOutcome::Accepted {
                schedule_persistence: true,
                close_dialog: false,
            }
        );
        assert_eq!(
            recovery_password_outcome(UnlockOutcome::AuthenticationFailed),
            PasswordSubmitOutcome::AuthenticationFailed
        );
        assert_eq!(
            recovery_password_outcome(UnlockOutcome::OperationFailed),
            PasswordSubmitOutcome::OperationFailed
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn dirty_protect_and_relock_wait_for_canonical_persistence_before_retrying() {
        let root = test_workspace("stillus-app-pending-security");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create pending-security workspace");
        fs::write(notes.join("Private.md"), "private body\n").expect("write private note");

        let mut model = AppModel::load(&root);
        model.apply(EditorCommand::Insert("dirty ".to_owned()));
        let search_generation = model.search_query_generation;
        let outcome = model.protect_selected(Some(stillus_secure::MasterPassword::new(
            "test master password".to_owned(),
        )));
        assert_eq!(outcome, SecurityActionOutcome::Pending);
        assert_eq!(model.search_query_generation, search_generation);
        assert!(
            model
                .pending_security_action
                .as_ref()
                .is_some_and(PendingSecurityAction::has_password)
        );

        finish_pending_persistence(&mut model);
        assert!(model.retry_pending_security_action());
        finish_pending_search_security(&mut model);
        assert!(model.pending_security_action.is_none());
        let workspace = model.workspace.as_ref().expect("workspace stays open");
        let protected_index = workspace
            .selected_note()
            .expect("protected note stays selected");
        assert_eq!(
            workspace.notes()[protected_index].protection,
            stillus_core::NoteProtection::Protected
        );
        assert!(workspace.document().is_none());

        model.open_note(protected_index);
        finish_pending_secure(&mut model);
        model.apply(EditorCommand::Insert("more ".to_owned()));
        assert_eq!(model.lock_selected(), SecurityActionOutcome::Pending);
        assert!(matches!(
            model.pending_security_action,
            Some(PendingSecurityAction::Lock { .. })
        ));
        finish_pending_persistence(&mut model);
        assert!(model.retry_pending_security_action());
        assert!(model.pending_security_action.is_none());
        assert!(
            model
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.document())
                .is_none()
        );

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove pending-security workspace");
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn failed_authentication_before_protect_keeps_password_dialog_open_for_retry() {
        let root = test_workspace("stillus-app-pending-auth");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create pending-auth workspace");
        fs::write(notes.join("A Private.md"), "private\n").expect("write private note");
        fs::write(notes.join("B Plain.md"), "plain\n").expect("write plain note");

        let mut first_session = AppModel::load(&root);
        assert_eq!(
            first_session.protect_selected(Some(stillus_secure::MasterPassword::new(
                "correct password".to_owned(),
            ))),
            SecurityActionOutcome::Pending
        );
        finish_pending_search_security(&mut first_session);
        first_session.shutdown_search_worker();
        drop(first_session);

        let mut model = AppModel::load(&root);
        let security = SecurityUi::new();
        security.open(PasswordDialogKind::ExistingProtection);
        assert!(security.entry.borrow_mut().push("wrong password"));
        security.busy.set(true);
        security.set_status(msg!(CheckingPassword));
        model.security_ui = Some(security.clone());
        model.apply(EditorCommand::Insert("dirty ".to_owned()));
        assert_eq!(
            model.protect_selected(Some(stillus_secure::MasterPassword::new(
                "wrong password".to_owned(),
            ))),
            SecurityActionOutcome::Pending
        );
        finish_pending_persistence(&mut model);
        assert!(model.retry_pending_security_action());
        finish_pending_search_security(&mut model);
        assert!(model.pending_security_action.is_none());
        assert_eq!(
            model.error.as_ref().map(ToString::to_string),
            Some(tr!(AuthenticationFailed))
        );
        assert_eq!(
            security.dialog.get_untracked(),
            Some(PasswordDialogKind::ExistingProtection)
        );
        assert!(!security.busy.get_untracked());
        assert!(security.entry.borrow().primary.is_empty());
        assert_eq!(
            security.feedback.get_untracked(),
            Some(PasswordFeedback::Error(msg!(AuthenticationFailed).into()))
        );
        let workspace = model.workspace.as_ref().expect("workspace stays open");
        let selected = workspace
            .selected_note()
            .expect("plain note stays selected");
        assert_eq!(
            workspace.notes()[selected].protection,
            stillus_core::NoteProtection::Plain
        );

        assert!(security.entry.borrow_mut().push("correct password"));
        security.busy.set(true);
        security.set_status(msg!(CheckingPassword));
        assert_eq!(
            model.protect_selected(Some(stillus_secure::MasterPassword::new(
                "correct password".to_owned(),
            ))),
            SecurityActionOutcome::Pending
        );
        finish_pending_search_security(&mut model);
        assert_eq!(security.dialog.get_untracked(), None);
        assert!(!security.busy.get_untracked());
        let workspace = model.workspace.as_ref().expect("workspace stays open");
        let selected = workspace
            .selected_note()
            .expect("retried note stays selected");
        assert_eq!(
            workspace.notes()[selected].protection,
            stillus_core::NoteProtection::Protected
        );

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove pending-auth workspace");
    }

    #[cfg(feature = "test-utils")]
    fn finish_pending_persistence(model: &mut AppModel) {
        let now_ms = model.now_ms().saturating_add(10_000);
        let workspace = model
            .application
            .test_workspace_mut()
            .expect("workspace stays open");
        let old_path = workspace
            .selected_note()
            .and_then(|index| workspace.notes().get(index))
            .map(|note| note.path.clone());
        workspace.retry_autosave(now_ms);
        for _ in 0..2 {
            let Some(job) = workspace
                .begin_persistence(now_ms, "2026-09-02T00:00:00Z".to_owned())
                .expect("pending persistence starts")
            else {
                break;
            };
            workspace
                .finish_persistence(job.execute())
                .expect("pending persistence finishes");
        }
        assert!(matches!(
            workspace
                .document()
                .expect("document remains open until security retry")
                .save_status(),
            stillus_core::SaveStatus::Clean { .. }
        ));
        let new_path = workspace
            .selected_note()
            .and_then(|index| workspace.notes().get(index))
            .map(|note| note.path.clone());
        if let (Some(old_path), Some(new_path)) = (old_path.as_deref(), new_path.as_deref())
            && old_path != new_path
            && let Some(action) = model.pending_security_action.as_mut()
        {
            action.replace_note_path(old_path, new_path);
        }
    }

    #[cfg(feature = "test-utils")]
    fn finish_pending_search_security(model: &mut AppModel) {
        let deadline = Deadline::new();
        while model.search_security_operation.is_some() || model.secure_worker_active {
            if model.secure_worker_active {
                let event = deadline.receive(&model.secure_receiver);
                match event {
                    Ok(SecureWorkerEvent::Progress(progress)) => {
                        model.finish_secure_progress(progress);
                    }
                    Ok(SecureWorkerEvent::Completed(completion)) => {
                        model.finish_secure_completion(*completion);
                    }
                    Err(error) => panic!("secure operation did not finish: {error}"),
                }
                continue;
            }
            let event = deadline.receive(&model.search_receiver);
            match event {
                Ok(SearchEvent::PurgeFinished {
                    operation_id,
                    result,
                }) if matches!(model.search_security_operation, Some(super::SearchSecurityOperation::Purging { operation_id: expected }) if expected == operation_id) =>
                {
                    assert!(model.finish_search_purge(operation_id, result));
                }
                Ok(SearchEvent::RestoreFinished {
                    operation_id,
                    result,
                }) if matches!(model.search_security_operation, Some(super::SearchSecurityOperation::Restoring { operation_id: expected, .. }) if expected == operation_id) =>
                {
                    assert!(model.finish_search_restore(operation_id, result));
                }
                Ok(_) => {}
                Err(error) => panic!("search security operation did not finish: {error}"),
            }
        }
    }

    #[cfg(feature = "test-utils")]
    fn finish_pending_secure(model: &mut AppModel) {
        let deadline = Deadline::new();
        loop {
            let event = deadline
                .receive(&model.secure_receiver)
                .expect("secure worker completes");
            match event {
                SecureWorkerEvent::Progress(progress)
                    if model.secure_operation_id == Some(progress.operation_id) =>
                {
                    assert!(model.finish_secure_progress(progress));
                }
                SecureWorkerEvent::Completed(completion)
                    if model.secure_operation_id == Some(completion.operation_id()) =>
                {
                    assert!(model.finish_secure_completion(*completion));
                    break;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn search_worker_acknowledges_async_purge_and_safe_restore() {
        let root = test_workspace("stillus-app-search-purge");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create search-purge test workspace");
        let target = notes.join("Needle.md");
        fs::write(&target, "private-marker-for-search\n").expect("write indexed note");

        let mut model = AppModel::load(&root);
        let purge_id = model.next_search_operation_id();
        model
            .search_sender
            .try_send(SearchCommand::Purge {
                operation_id: purge_id,
                note_path: target.clone(),
            })
            .expect("queue purge without waiting for rebuild");
        wait_for_search_operation(&mut model, purge_id, false)
            .expect("search worker acknowledges purge");
        assert!(search_results_for(&mut model, "private-marker-for-search").is_empty());

        let restore_id = model.next_search_operation_id();
        model
            .search_sender
            .try_send(SearchCommand::RestoreAfterFailedPurge {
                operation_id: restore_id,
                note_path: target.clone(),
            })
            .expect("queue restore without waiting for rebuild");
        wait_for_search_operation(&mut model, restore_id, true)
            .expect("search worker acknowledges safe restore");
        let restored = search_results_for(&mut model, "private-marker-for-search");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].title, "private-marker-for-search");

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove search-purge test workspace");
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn protected_note_switch_reuses_only_the_authenticated_process_session() {
        let root = test_workspace("stillus-app-protected-session");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create protected-session workspace");
        fs::write(notes.join("A.md"), "alpha\n").expect("write first note");
        fs::write(notes.join("B.md"), "bravo\n").expect("write second note");
        fs::write(notes.join("C.md"), "charlie\n").expect("write third note");

        let mut model = AppModel::load(&root);
        let first_protected = model
            .application
            .test_workspace_mut()
            .expect("workspace opens")
            .protect_selected(Some(stillus_secure::MasterPassword::new(
                "test master password".to_owned(),
            )))
            .expect("protect first note");
        let second_index = model
            .workspace
            .as_ref()
            .expect("workspace stays open")
            .notes()
            .iter()
            .position(|note| note.title == "bravo")
            .expect("second plain note remains visible");
        model.open_note(second_index);
        model
            .application
            .test_workspace_mut()
            .expect("workspace stays open")
            .protect_selected(None)
            .expect("protect second note with process session");

        let plain_index = model
            .workspace
            .as_ref()
            .expect("workspace stays open")
            .notes()
            .iter()
            .position(|note| note.title == "charlie")
            .expect("third plain note remains visible");
        model.open_note(plain_index);
        assert_eq!(
            model
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.document())
                .map(|document| document.title()),
            Some("charlie")
        );
        let workspace = model.workspace.as_ref().expect("workspace stays open");
        assert!(workspace.has_master_password());
        assert_eq!(
            protection_password_dialog(workspace),
            super::PasswordDialogKind::ExistingProtection
        );

        let first_index = model
            .workspace
            .as_ref()
            .expect("workspace stays open")
            .notes()
            .iter()
            .position(|note| note.path == first_protected)
            .expect("first protected note remains present");
        model.open_note(first_index);
        let workspace = model.workspace.as_ref().expect("workspace stays open");
        assert_eq!(workspace.selected_note(), Some(first_index));
        assert!(workspace.document().is_none());
        assert!(model.secure_worker_active);
        assert_eq!(
            protection_action_state(&model),
            super::ProtectionActionState::Decrypting
        );
        assert_eq!(
            super::protected_placeholder(&model),
            Some(super::ProtectedPlaceholder::Decrypting)
        );
        // The card owns the wording; the text surface must stay empty so no
        // plaintext can leak through it while the envelope is open.
        assert!(render_editor(&model).is_empty());
        finish_pending_secure(&mut model);
        assert!(model.unlock_request.is_none());
        assert_eq!(
            model
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.document())
                .map(|document| document.title()),
            Some("alpha")
        );

        model.shutdown_search_worker();
        drop(model);

        let mut reopened = AppModel::load(&root);
        let first_index = reopened
            .workspace
            .as_ref()
            .expect("workspace reopens")
            .notes()
            .iter()
            .position(|note| note.path == first_protected)
            .expect("protected note remains present after restart");
        reopened.open_note(first_index);
        assert_eq!(reopened.unlock_request, Some(first_index));
        assert!(
            reopened
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.document())
                .is_none()
        );

        reopened.shutdown_search_worker();
        drop(reopened);
        fs::remove_dir_all(root).expect("remove protected-session workspace");
    }

    fn search_results_for(model: &mut AppModel, query: &str) -> Vec<stillus_search::SearchResult> {
        model.submit_search(query.to_owned());
        let generation = model.search_query_generation;
        Deadline::new()
            .matching(&model.search_receiver, |event| match event {
                SearchEvent::Results {
                    generation: incoming,
                    results,
                } if incoming == generation => Some(results),
                _ => None,
            })
            .expect("search worker answers the requested query")
    }

    fn wait_for_search_operation(
        model: &mut AppModel,
        operation_id: u64,
        restore: bool,
    ) -> Result<(), String> {
        Deadline::new()
            .matching(&model.search_receiver, |event| match event {
                SearchEvent::PurgeFinished {
                    operation_id: incoming,
                    result,
                } if !restore && incoming == operation_id => Some(result),
                SearchEvent::RestoreFinished {
                    operation_id: incoming,
                    result,
                } if restore && incoming == operation_id => Some(result),
                _ => None,
            })
            .map_err(|error| format!("search worker did not answer operation: {error}"))?
    }

    #[test]
    fn stale_search_results_never_replace_the_latest_query() {
        assert!(is_current_search_generation(42, 42));
        assert!(!is_current_search_generation(42, 41));
        assert!(!is_current_search_generation(42, 43));
    }

    #[test]
    fn search_selection_requires_the_current_query_and_rendered_generation() {
        let root = test_workspace("stillus-app-search-selection");
        fs::create_dir_all(root.join("notes")).unwrap();
        fs::write(root.join("notes/A.md"), "alpha\n").unwrap();
        fs::write(root.join("notes/B.md"), "bravo\n").unwrap();
        let mut model = AppModel::load(&root);
        model.shutdown_search_worker();
        // Drive responses explicitly: no worker scheduling or sleeps decide
        // whether an old pointer event or an early Enter is accepted.
        let (sender, commands) = std::sync::mpsc::sync_channel(64);
        model.search_sender = sender;
        model.search_indexing = false;
        model.search_error = None;
        let result = stillus_search::SearchResult {
            relative_path: "notes/B.md".to_owned(),
            title: "bravo".to_owned(),
            tags: Vec::new(),
            snippet: String::new(),
            match_kind: stillus_search::MatchKind::Title,
            score: 1.0,
        };

        model.submit_search("br".to_owned());
        let first = match Deadline::new().receive(&commands).unwrap() {
            SearchCommand::Query { generation, .. } => generation,
            _ => panic!("expected query"),
        };
        assert!(model.accept_search_results(first, vec![result.clone()]));
        assert_eq!(model.search_result_generation("br"), Some(first));
        // The input can already have changed before its reactive effect runs.
        assert_eq!(model.search_result_generation("bravo"), None);
        assert!(!model.open_search_result(first, "bravo", "notes/B.md"));

        model.submit_search("bravo".to_owned());
        let second = model.search_query_generation;
        assert!(model.search_results.is_empty());
        assert_eq!(model.search_result_generation("bravo"), None);
        assert!(!model.open_search_result(first, "bravo", "notes/B.md"));
        assert!(!model.accept_search_results(first, vec![result.clone()]));
        assert_eq!(model.search_result_generation("bravo"), None);
        assert!(model.accept_search_results(second, vec![result.clone()]));
        assert!(!model.accept_search_results(first, Vec::new()));
        assert_eq!(model.search_results, vec![result.clone()]);
        // Even when the same path occurs in both queries, the old row is inert.
        assert!(!model.open_search_result(first, "bravo", "notes/B.md"));
        assert!(!model.open_search_result(second, "bravo", "notes/A.md"));
        assert_eq!(model.workspace.as_ref().unwrap().selected_note(), Some(0));
        assert!(model.open_search_result(second, "bravo", "notes/B.md"));
        assert_eq!(model.workspace.as_ref().unwrap().selected_note(), Some(1));

        // Reconciliation of the same query invalidates its earlier rows too.
        model.submit_search("bravo".to_owned());
        assert!(!model.open_search_result(second, "bravo", "notes/B.md"));
        let reconciled = model.search_query_generation;
        assert!(model.accept_search_results(reconciled, vec![result.clone()]));
        assert!(model.open_search_result(reconciled, "bravo", "notes/B.md"));

        model.invalidate_search_projection();
        model.search_indexing = true;
        let rebuilding = model.search_query_generation;
        assert!(!model.accept_search_results(reconciled, vec![result.clone()]));
        assert!(!model.accept_search_results(rebuilding, vec![result.clone()]));
        assert!(!model.open_search_result(reconciled, "bravo", "notes/B.md"));
        model.search_indexing = false;
        model.submit_search("bravo".to_owned());
        let rebuilt = model.search_query_generation;
        assert!(model.accept_search_results(rebuilt, vec![result]));
        assert!(model.open_search_result(rebuilt, "bravo", "notes/B.md"));

        model.submit_search(String::new());
        assert!(model.search_results.is_empty());
        assert_eq!(model.search_result_generation(""), None);
        assert!(!model.open_search_result(rebuilt, "", "notes/B.md"));
        drop(model);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_worker_coalesces_a_queued_query_burst_to_the_latest_request() {
        let root = test_workspace("stillus-app-search-coalesce");
        fs::create_dir_all(root.join("notes")).expect("create coalescing workspace");
        fs::write(root.join("notes/Needle.md"), "latestquerymarker\n")
            .expect("write searchable note");

        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let (event_sender, event_receiver) = std::sync::mpsc::sync_channel(64);
        for (generation, query) in [(1, "first"), (2, "second"), (3, "latestquerymarker")] {
            command_sender
                .send(SearchCommand::Query {
                    generation,
                    query: query.to_owned(),
                })
                .expect("queue query before worker starts");
        }
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            search_worker(worker_root, command_receiver, event_sender, false);
        });

        let deadline = Deadline::new();
        let (generation, results) = loop {
            match deadline.receive(&event_receiver) {
                Ok(SearchEvent::Results {
                    generation,
                    results,
                }) => break (generation, results),
                Ok(_) => {}
                Err(error) => panic!("search worker did not return coalesced query: {error}"),
            }
        };
        assert_eq!(generation, 3);
        assert_eq!(results.len(), 1);

        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        command_sender
            .send(SearchCommand::Shutdown(finished_sender))
            .expect("queue worker shutdown");
        Deadline::new()
            .receive(&finished_receiver)
            .expect("worker acknowledges shutdown");
        worker.join().expect("search worker exits cleanly");
        fs::remove_dir_all(root).expect("remove coalescing workspace");
    }

    #[test]
    fn security_barrier_drops_cached_results_and_rejects_stale_worker_results() {
        let root = test_workspace("stillus-app-search-barrier");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create search-barrier workspace");
        fs::write(notes.join("Private.md"), "search-plaintext-marker\n")
            .expect("write indexed note");
        let mut model = AppModel::load(&root);
        model.search_results = search_results_for(&mut model, "search-plaintext-marker");
        let stale_generation = model.search_query_generation;
        assert!(!model.search_results.is_empty());

        model.invalidate_search_projection();
        assert!(model.search_results.is_empty());
        assert!(model.search_query_generation > stale_generation);
        assert!(!is_current_search_generation(
            model.search_query_generation,
            stale_generation
        ));

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove search-barrier workspace");
    }

    #[test]
    fn note_click_queues_the_target_while_the_current_note_is_dirty() {
        let root = test_workspace("stillus-app-pending-note");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create pending-note test workspace");
        fs::write(notes.join("A Alpha.md"), "alpha\n").expect("write first note");
        let target = notes.join("B Bravo.md");
        fs::write(&target, "bravo\n").expect("write second note");

        let mut model = AppModel::load(&root);
        model.apply(EditorCommand::Insert("dirty".to_owned()));
        model.open_note(1);

        let workspace = model.workspace.as_ref().expect("workspace stays open");
        assert_eq!(workspace.selected_note(), Some(0));
        assert_eq!(model.pending_note_path.as_deref(), Some(target.as_path()));
        assert!(model.error.is_none());
        assert!(workspace.next_autosave_deadline().is_some());

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove pending-note test workspace");
    }

    #[test]
    fn sidebar_filter_matches_special_groups_and_category_subtrees() {
        let tags = vec!["Work".to_owned(), "Work/Planning".to_owned()];
        assert!(note_matches_filter(
            &tags,
            false,
            false,
            &SidebarFilter::All
        ));
        assert!(!note_matches_filter(
            &tags,
            false,
            false,
            &SidebarFilter::Favorites
        ));
        assert!(note_matches_filter(
            &tags,
            true,
            false,
            &SidebarFilter::Favorites
        ));
        assert!(note_matches_filter(
            &tags,
            false,
            false,
            &SidebarFilter::Tag("Work".to_owned())
        ));
        assert!(note_matches_filter(
            &tags,
            false,
            false,
            &SidebarFilter::Tag("Work/Planning".to_owned())
        ));
        assert!(!note_matches_filter(
            &tags,
            true,
            false,
            &SidebarFilter::Tag("work".to_owned())
        ));
        assert!(!note_matches_filter(
            &tags,
            true,
            false,
            &SidebarFilter::Tag("Planning".to_owned())
        ));
        assert!(!note_matches_filter(
            &["Work∕Planning".to_owned()],
            false,
            false,
            &SidebarFilter::Tag("Work".to_owned())
        ));
        assert!(!note_matches_filter(&tags, true, true, &SidebarFilter::All));
        assert!(note_matches_filter(
            &tags,
            true,
            true,
            &SidebarFilter::Trash
        ));
    }

    #[test]
    fn toolbar_controls_are_shared_between_engine_surfaces() {
        for action in [
            ToolbarAction::Refresh,
            ToolbarAction::Rename,
            ToolbarAction::Categories,
            ToolbarAction::Pin,
            ToolbarAction::Favorite,
            ToolbarAction::Delete,
            ToolbarAction::Restore,
        ] {
            assert!(!toolbar_action_icon(action).is_empty());
            for subject in [ToolbarSubject::Note, ToolbarSubject::Feed] {
                assert!(!toolbar_action_title(action, subject, false).is_empty());
                assert!(!toolbar_action_title(action, subject, true).is_empty());
            }
        }
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Rename),
            super::ButtonAction::Edit.icon()
        );
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Pin),
            super::ButtonAction::Pin.icon()
        );
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Favorite),
            super::ButtonAction::Favorite.icon()
        );
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Delete),
            super::ButtonAction::Delete.icon()
        );
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Restore),
            super::ICON_RECOVER
        );
        assert_eq!(
            toolbar_action_icon(ToolbarAction::Categories),
            super::ICON_TAG
        );
    }

    #[test]
    fn toolbar_titles_name_the_subject_of_the_surface() {
        assert_eq!(
            toolbar_action_title(ToolbarAction::Pin, ToolbarSubject::Note, false),
            "Pin note"
        );
        assert_eq!(
            toolbar_action_title(ToolbarAction::Pin, ToolbarSubject::Feed, true),
            "Unpin feed"
        );
        assert_eq!(
            toolbar_action_title(ToolbarAction::Delete, ToolbarSubject::Note, false),
            "Move note to trash"
        );
        assert_eq!(
            toolbar_action_title(ToolbarAction::Restore, ToolbarSubject::Feed, false),
            "Restore feed"
        );
        assert_eq!(
            toolbar_action_title(ToolbarAction::Favorite, ToolbarSubject::Feed, true),
            tr!(RemoveFavorite)
        );
        assert_eq!(
            toolbar_action_title(ToolbarAction::Categories, ToolbarSubject::Note, false),
            tr!(ManageTags)
        );
    }

    #[test]
    fn only_delete_and_restore_skip_the_lit_state() {
        assert!(toolbar_action_is_toggle(ToolbarAction::Pin));
        assert!(toolbar_action_is_toggle(ToolbarAction::Favorite));
        assert!(toolbar_action_is_toggle(ToolbarAction::Refresh));
        assert!(toolbar_action_is_toggle(ToolbarAction::Rename));
        assert!(toolbar_action_is_toggle(ToolbarAction::Categories));
        assert!(!toolbar_action_is_toggle(ToolbarAction::Delete));
        assert!(!toolbar_action_is_toggle(ToolbarAction::Restore));
        assert!(matches!(
            toolbar_action_tone(ToolbarAction::Delete),
            IconButtonTone::Danger
        ));
        assert!(matches!(
            toolbar_action_tone(ToolbarAction::Restore),
            IconButtonTone::Secondary
        ));
    }

    #[test]
    fn delete_and_restore_share_one_toolbar_slot() {
        let declared = [
            ToolbarAction::Refresh,
            ToolbarAction::Rename,
            ToolbarAction::Categories,
            ToolbarAction::Pin,
            ToolbarAction::Favorite,
            ToolbarAction::Delete,
            ToolbarAction::Restore,
        ];
        assert_eq!(
            visible_toolbar_actions(&declared, false),
            vec![
                ToolbarAction::Refresh,
                ToolbarAction::Rename,
                ToolbarAction::Categories,
                ToolbarAction::Pin,
                ToolbarAction::Favorite,
                ToolbarAction::Delete,
            ]
        );
        assert_eq!(
            visible_toolbar_actions(&declared, true),
            vec![
                ToolbarAction::Refresh,
                ToolbarAction::Rename,
                ToolbarAction::Categories,
                ToolbarAction::Pin,
                ToolbarAction::Favorite,
                ToolbarAction::Restore,
            ]
        );
        assert!(visible_toolbar_actions(&[], false).is_empty());
    }

    #[test]
    fn rss_toolbar_ignores_queued_updates_after_panel_disposal() {
        use floem::reactive::{Scope, with_scope};
        use std::{cell::RefCell, rc::Rc};

        let root = Scope::new();
        let revision = root.create_rw_signal(0_u64);
        let model = Rc::new(RefCell::new(AppModel::unloaded()));
        let item_id = stillus_core::ItemId::new("rss/test").unwrap();
        let declared = [
            ToolbarAction::Refresh,
            ToolbarAction::Rename,
            ToolbarAction::Categories,
            ToolbarAction::Pin,
            ToolbarAction::Favorite,
            ToolbarAction::Delete,
            ToolbarAction::Restore,
        ];

        for deleted in [false, true, false] {
            let panel_scope = root.create_child();
            let bar = || super::ToolbarEditBar {
                open: panel_scope.create_rw_signal(false),
                value: panel_scope.create_rw_signal(String::new()),
                label: Key::NewTitle,
                placeholder: Key::NewTitle,
            };
            let signals = super::RssToolbarSignals {
                filters_open: super::create_rw_signal(false),
                rename: bar(),
                categories: bar(),
            };
            let build_controls = || {
                super::rss_toolbar_controls(
                    &declared,
                    deleted,
                    model.clone(),
                    item_id.clone(),
                    revision,
                    signals,
                    super::Palette::new(),
                )
            };
            let mounted = with_scope(panel_scope, build_controls);
            assert_eq!(mounted.id().children().len(), 6);
            signals.rename.open.set(true);

            // Switching to a document disposes the feed signals before an
            // already queued DynamicContainer update can build its controls.
            panel_scope.dispose();
            assert!(signals.rename.open.try_get_untracked().is_none());
            assert_eq!(revision.get_untracked(), 0);
            let late_scope = root.create_child();
            let late = with_scope(late_scope, build_controls);
            assert!(late.id().children().is_empty());
            late_scope.dispose();
        }
        root.dispose();
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn autosave_ignores_secure_completion_after_window_disposal() {
        use floem::reactive::{Scope, with_scope};
        use std::{cell::RefCell, rc::Rc};

        let root = test_workspace("stillus-disposed-window");
        fs::create_dir_all(root.join("notes")).unwrap();
        let note_path = root.join("notes/Private.md");
        fs::write(&note_path, "late completion fixture\n").unwrap();
        let scope = Scope::new();
        let revision = scope.create_rw_signal(0_u64);
        let mut model = AppModel::unloaded();
        model
            .application
            .test_set_workspace(WorkspaceSession::open(&root).unwrap());
        model
            .application
            .test_workspace_mut()
            .unwrap()
            .open_note(0)
            .unwrap();
        model.security_ui = Some(with_scope(scope, SecurityUi::new));
        let job = model
            .application
            .test_workspace_mut()
            .unwrap()
            .begin_protect_selected(Some(MasterPassword::new("test password".to_owned())))
            .unwrap();
        model.secure_operation_id = Some(job.operation_id());
        model.secure_worker_active = true;
        model.secure_ui_operation = Some(SecureUiOperation::Protect {
            action: PendingSecurityAction::Protect {
                note_path: note_path.clone(),
                password: None,
            },
            note_path: note_path.clone(),
        });
        model
            .secure_sender
            .send(SecureWorkerEvent::Completed(Box::new(job.execute())))
            .unwrap();
        let committed = fs::read(&note_path).unwrap();
        let generation = model.autosave_generation;
        let model = Rc::new(RefCell::new(model));

        scope.dispose();
        super::autosave_tick(model.clone(), revision, generation);

        assert!(model.borrow().secure_receiver.try_recv().is_ok());
        assert!(!model.borrow().save_worker_active);
        assert_eq!(fs::read(&note_path).unwrap(), committed);
        drop(model);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn category_input_drops_blanks_and_repeats_and_keeps_order() {
        assert_eq!(
            parsed_category_list("  Работа , Новости/Технологии ,, Работа "),
            vec!["Работа".to_owned(), "Новости/Технологии".to_owned()]
        );
        assert!(parsed_category_list("  , ,").is_empty());
    }

    #[test]
    fn sidebar_indicators_never_include_save_loader() {
        assert_eq!(
            sidebar_note_indicator_icons(false, true, true),
            vec![
                super::ButtonAction::Pin.icon(),
                super::ButtonAction::Favorite.icon()
            ]
        );
        assert_eq!(
            sidebar_note_indicator_icons(true, false, false),
            vec![super::ICON_LOCK]
        );
        assert!(sidebar_note_indicator_icons(false, false, false).is_empty());
    }

    #[test]
    fn tag_filter_projection_preserves_order_and_does_not_duplicate_rows() {
        let tag_sets = [
            vec![],
            vec!["Work".to_owned()],
            vec!["Personal".to_owned(), "Work".to_owned()],
            vec!["Work".to_owned(), "Work".to_owned()],
        ];
        let notes = || {
            tag_sets
                .iter()
                .enumerate()
                .map(|(index, tags)| (tags.as_slice(), index % 2 == 0, false))
        };

        assert_eq!(
            matching_tag_indices(notes(), &SidebarFilter::All),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            matching_tag_indices(notes(), &SidebarFilter::Favorites),
            vec![0, 2]
        );
        assert_eq!(
            matching_tag_indices(notes(), &SidebarFilter::Tag("Work".to_owned())),
            vec![1, 2, 3]
        );
        assert_eq!(
            matching_tag_indices(notes(), &SidebarFilter::Tag("Personal".to_owned())),
            vec![2]
        );
        assert!(matching_tag_indices(notes(), &SidebarFilter::Tag("work".to_owned())).is_empty());
        assert!(matching_tag_indices(notes(), &SidebarFilter::Tag(String::new())).is_empty());
    }

    #[test]
    fn tag_autocomplete_uses_workspace_order_prefix_and_excludes_assigned_tags() {
        let categories = ["Personal", "Planning", "Work", "workshop", "Задачи"];
        let assigned = vec!["Planning".to_owned(), "Work".to_owned()];
        assert_eq!(tag_suggestions(categories, &assigned, "  wo"), ["workshop"]);
        assert_eq!(tag_suggestions(categories, &assigned, "зад"), ["Задачи"]);
        assert!(tag_suggestions(categories, &assigned, "   ").is_empty());
        assert!(tag_suggestions(categories, &assigned, "missing").is_empty());
    }

    #[test]
    fn tag_autocomplete_keyboard_clamps_and_submission_prefers_highlight() {
        let suggestions = vec!["Personal".to_owned(), "Planning".to_owned()];
        assert_eq!(
            move_tag_suggestion_highlight(None, 2, TagSuggestionDirection::Next),
            Some(0)
        );
        assert_eq!(
            move_tag_suggestion_highlight(None, 2, TagSuggestionDirection::Previous),
            Some(1)
        );
        assert_eq!(
            move_tag_suggestion_highlight(Some(1), 2, TagSuggestionDirection::Next),
            Some(1)
        );
        assert_eq!(
            move_tag_suggestion_highlight(Some(0), 2, TagSuggestionDirection::Previous),
            Some(0)
        );
        assert_eq!(
            move_tag_suggestion_highlight(Some(7), 2, TagSuggestionDirection::Previous),
            Some(0)
        );
        assert_eq!(
            move_tag_suggestion_highlight(Some(0), 0, TagSuggestionDirection::Next),
            None
        );
        assert_eq!(
            tag_submission("custom", &suggestions, Some(1)),
            Some("Planning".to_owned())
        );
        assert_eq!(
            tag_submission("  Custom  ", &suggestions, None),
            Some("Custom".to_owned())
        );
        assert_eq!(tag_submission("   ", &suggestions, None), None);
    }

    #[test]
    fn app_tag_mutation_reports_duplicate_and_missing_as_no_ops() {
        let root = test_workspace("stillus-app-tag-outcome");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create tag-outcome workspace");
        let note = notes.join("Tagged.md");
        fs::write(&note, "---\ntitle: Tagged\ntags: [Work]\n---\nbody\n")
            .expect("write tagged note");

        let mut model = AppModel::load(&root);
        let original = fs::read(&note).expect("read original note");
        assert!(!model.start_optional_metadata_job(Ok(None)));
        assert!(!model.add_tag_selected("Work"));
        assert_eq!(fs::read(&note).expect("read duplicate no-op"), original);
        assert!(!model.remove_tag_selected("Missing"));
        assert_eq!(fs::read(&note).expect("read missing no-op"), original);
        assert!(model.add_tag_selected("New"));
        assert!(model.remove_tag_selected("New"));

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove tag-outcome workspace");
    }

    #[test]
    fn sidebar_tree_lists_notes_under_every_expanded_group_in_sidebar_order() {
        let tag_sets = [
            vec!["Work".to_owned(), "Planning".to_owned()],
            vec!["Personal".to_owned()],
            vec!["Work".to_owned()],
        ];
        let notes = tag_sets
            .iter()
            .enumerate()
            .map(|(index, tags)| (tags.as_slice(), index == 0, false))
            .collect::<Vec<_>>();
        let categories = ["Personal", "Planning", "Work"];
        let group = |filter: SidebarFilter, count: usize| {
            let title = match &filter {
                SidebarFilter::All => tr!(All),
                SidebarFilter::Favorites => tr!(Favorites),
                SidebarFilter::Tag(tag) => tag.clone(),
                SidebarFilter::Trash => tr!(Trash),
            };
            SidebarRow::Group {
                filter,
                title,
                count,
                depth: 0,
            }
        };
        let note = |parent: SidebarFilter, index: usize| SidebarRow::Note {
            parent,
            index,
            depth: 0,
        };
        let mut state = SidebarState::default();

        assert_eq!(
            sidebar_rows(&notes, categories, &state),
            vec![
                group(SidebarFilter::Favorites, 1),
                SidebarRow::Separator,
                group(SidebarFilter::Tag("Personal".to_owned()), 1),
                group(SidebarFilter::Tag("Planning".to_owned()), 1),
                group(SidebarFilter::Tag("Work".to_owned()), 2),
                group(SidebarFilter::All, 3),
                note(SidebarFilter::All, 0),
                note(SidebarFilter::All, 1),
                note(SidebarFilter::All, 2),
                group(SidebarFilter::Trash, 0),
            ]
        );

        assert_eq!(
            state.toggle_group(SidebarFilter::Favorites),
            SidebarGroupToggle::Opened
        );
        assert_eq!(
            state.toggle_group(SidebarFilter::Tag("Work".to_owned())),
            SidebarGroupToggle::Opened
        );
        assert_eq!(
            sidebar_rows(&notes, categories, &state),
            vec![
                group(SidebarFilter::Favorites, 1),
                note(SidebarFilter::Favorites, 0),
                SidebarRow::Separator,
                group(SidebarFilter::Tag("Personal".to_owned()), 1),
                group(SidebarFilter::Tag("Planning".to_owned()), 1),
                group(SidebarFilter::Tag("Work".to_owned()), 2),
                note(SidebarFilter::Tag("Work".to_owned()), 0),
                note(SidebarFilter::Tag("Work".to_owned()), 2),
                group(SidebarFilter::All, 3),
                note(SidebarFilter::All, 0),
                note(SidebarFilter::All, 1),
                note(SidebarFilter::All, 2),
                group(SidebarFilter::Trash, 0),
            ]
        );

        assert_eq!(
            state.toggle_group(SidebarFilter::All),
            SidebarGroupToggle::Closed
        );
        assert_eq!(
            state.toggle_group(SidebarFilter::Favorites),
            SidebarGroupToggle::Closed
        );
        assert_eq!(
            state.toggle_group(SidebarFilter::Tag("Work".to_owned())),
            SidebarGroupToggle::Closed
        );
        assert_eq!(
            sidebar_rows(&notes, categories, &state)
                .into_iter()
                .filter(|row| matches!(row, SidebarRow::Note { .. }))
                .collect::<Vec<_>>(),
            Vec::<SidebarRow>::new()
        );
        assert_eq!(
            sidebar_rows(&notes, [], &state),
            vec![
                group(SidebarFilter::Favorites, 1),
                group(SidebarFilter::All, 3),
                group(SidebarFilter::Trash, 0)
            ]
        );
    }

    #[test]
    fn sidebar_tree_routes_deleted_notes_only_to_the_final_trash_group() {
        let active_tags = vec!["Work".to_owned()];
        let deleted_tags = vec!["Work".to_owned()];
        let notes = [
            (active_tags.as_slice(), true, false),
            (deleted_tags.as_slice(), true, true),
        ];
        let mut state = SidebarState::default();
        state.toggle_group(SidebarFilter::Favorites);
        state.toggle_group(SidebarFilter::Tag("Work".to_owned()));
        state.toggle_group(SidebarFilter::Trash);
        let group = |filter: SidebarFilter, count: usize| {
            let title = match &filter {
                SidebarFilter::All => tr!(All),
                SidebarFilter::Favorites => tr!(Favorites),
                SidebarFilter::Tag(tag) => tag.clone(),
                SidebarFilter::Trash => tr!(Trash),
            };
            SidebarRow::Group {
                filter,
                title,
                count,
                depth: 0,
            }
        };
        let note = |parent: SidebarFilter, index: usize| SidebarRow::Note {
            parent,
            index,
            depth: 0,
        };

        assert_eq!(
            sidebar_rows(&notes, ["Work"], &state),
            vec![
                group(SidebarFilter::Favorites, 1),
                note(SidebarFilter::Favorites, 0),
                SidebarRow::Separator,
                group(SidebarFilter::Tag("Work".to_owned()), 1),
                note(SidebarFilter::Tag("Work".to_owned()), 0),
                group(SidebarFilter::All, 1),
                note(SidebarFilter::All, 0),
                group(SidebarFilter::Trash, 1),
                note(SidebarFilter::Trash, 1),
            ]
        );
    }

    #[test]
    fn hierarchical_categories_merge_virtual_and_exact_paths_with_recursive_rows() {
        let tag_sets = [
            vec!["Parent".to_owned()],
            vec!["Parent/Child".to_owned()],
            vec!["Parent/Child/Leaf".to_owned()],
            vec!["Parent/Other".to_owned(), "Parent/Child".to_owned()],
            vec!["Zoo".to_owned()],
        ];
        let notes = tag_sets
            .iter()
            .map(|tags| (tags.as_slice(), false, false))
            .collect::<Vec<_>>();
        let categories = [
            "Zoo",
            "Parent/Other",
            "Parent/Child/Leaf",
            "Parent",
            "Parent/Child",
        ];

        let tree = sidebar_category_tree(&notes, categories, &[]);
        assert_eq!(
            tree.iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["Parent", "Zoo"]
        );
        let parent = &tree[0];
        assert_eq!(parent.label, "Parent");
        assert_eq!(parent.direct_notes, vec![0]);
        assert_eq!(parent.subtree_notes, vec![0, 1, 2, 3]);
        assert_eq!(
            parent
                .children
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["Parent/Child", "Parent/Other"]
        );
        assert_eq!(parent.children[0].subtree_notes, vec![1, 2, 3]);
        assert_eq!(parent.children[1].subtree_notes, vec![3]);

        let mut state = SidebarState::default();
        state
            .expanded
            .insert(SidebarFilter::Tag("Parent".to_owned()));
        state
            .expanded
            .insert(SidebarFilter::Tag("Parent/Child".to_owned()));
        let category_rows = sidebar_rows(&notes, categories, &state)
            .into_iter()
            .skip_while(|row| !matches!(row, SidebarRow::Separator))
            .skip(1)
            .take_while(|row| {
                !matches!(
                    row,
                    SidebarRow::Group {
                        filter: SidebarFilter::All,
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            category_rows,
            vec![
                SidebarRow::Group {
                    filter: SidebarFilter::Tag("Parent".to_owned()),
                    title: "Parent".to_owned(),
                    count: 4,
                    depth: 0,
                },
                SidebarRow::Group {
                    filter: SidebarFilter::Tag("Parent/Child".to_owned()),
                    title: "Child".to_owned(),
                    count: 3,
                    depth: 1,
                },
                SidebarRow::Group {
                    filter: SidebarFilter::Tag("Parent/Child/Leaf".to_owned()),
                    title: "Leaf".to_owned(),
                    count: 1,
                    depth: 2,
                },
                SidebarRow::Note {
                    parent: SidebarFilter::Tag("Parent/Child".to_owned()),
                    index: 1,
                    depth: 1,
                },
                SidebarRow::Note {
                    parent: SidebarFilter::Tag("Parent/Child".to_owned()),
                    index: 3,
                    depth: 1,
                },
                SidebarRow::Group {
                    filter: SidebarFilter::Tag("Parent/Other".to_owned()),
                    title: "Other".to_owned(),
                    count: 1,
                    depth: 1,
                },
                SidebarRow::Note {
                    parent: SidebarFilter::Tag("Parent".to_owned()),
                    index: 0,
                    depth: 0,
                },
                SidebarRow::Group {
                    filter: SidebarFilter::Tag("Zoo".to_owned()),
                    title: "Zoo".to_owned(),
                    count: 1,
                    depth: 0,
                },
            ]
        );
    }

    #[test]
    fn malformed_slash_tags_remain_flat_and_visual_depth_is_capped() {
        for category in ["/Child", "Parent/", "Parent//Child", "Parent∕Child"] {
            assert_eq!(category_path_segments(category), vec![category]);
        }
        assert_eq!(
            category_path_segments("Parent/Child/Leaf"),
            vec!["Parent", "Child", "Leaf"]
        );
        let notes: Vec<(&[String], bool, bool)> = Vec::new();
        let tree = sidebar_category_tree(
            &notes,
            ["/Child", "Parent/", "Parent//Child", "Parent∕Child"],
            &[],
        );
        assert_eq!(
            tree.iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/Child", "Parent/", "Parent//Child", "Parent∕Child"]
        );
        assert!(tree.iter().all(|node| node.children.is_empty()));
        assert_eq!(sidebar_tree_indent(0), 0.0);
        assert_eq!(sidebar_tree_indent(3), 48.0);
        assert_eq!(sidebar_tree_indent(7), 96.0);
    }

    #[test]
    fn sidebar_state_toggles_independently_and_reconciles_vanished_categories() {
        let mut state = SidebarState::default();
        let work = SidebarFilter::Tag("Work".to_owned());
        let personal = SidebarFilter::Tag("Personal".to_owned());
        assert!(state.is_expanded(&SidebarFilter::All));

        assert_eq!(state.toggle_group(work.clone()), SidebarGroupToggle::Opened);
        assert_eq!(
            state.toggle_group(personal.clone()),
            SidebarGroupToggle::Opened
        );
        assert!(state.is_expanded(&SidebarFilter::All));
        assert!(state.is_expanded(&work));
        assert!(state.is_expanded(&personal));
        assert_eq!(state.creation_group, personal);

        state.use_group(work.clone());
        assert_eq!(state.creation_group, work);
        assert_eq!(state.toggle_group(work.clone()), SidebarGroupToggle::Closed);
        assert!(!state.is_expanded(&work));
        assert!(state.is_expanded(&personal));
        assert_eq!(state.creation_group, SidebarFilter::All);

        state.use_group(personal.clone());
        state.reconcile_categories(["Work"]);
        assert!(!state.is_expanded(&personal));
        assert_eq!(state.creation_group, SidebarFilter::All);
        assert!(state.is_expanded(&SidebarFilter::All));

        assert_eq!(
            state.toggle_group(SidebarFilter::Trash),
            SidebarGroupToggle::Opened
        );
        assert_eq!(state.creation_group, SidebarFilter::All);
        state.use_group(SidebarFilter::Trash);
        assert_eq!(state.creation_group, SidebarFilter::All);

        assert_eq!(
            state.toggle_group(SidebarFilter::All),
            SidebarGroupToggle::Closed
        );
        assert_eq!(
            state.toggle_group(SidebarFilter::Trash),
            SidebarGroupToggle::Closed
        );
        assert!(state.expanded.is_empty());
    }

    #[test]
    fn collapsing_ancestor_preserves_descendant_expansion_and_resets_creation_group() {
        let mut state = SidebarState::default();
        let parent = SidebarFilter::Tag("Parent".to_owned());
        let child = SidebarFilter::Tag("Parent/Child".to_owned());
        let leaf = SidebarFilter::Tag("Parent/Child/Leaf".to_owned());

        assert_eq!(
            state.toggle_group(parent.clone()),
            SidebarGroupToggle::Opened
        );
        assert_eq!(state.creation_group, parent);
        assert_eq!(
            state.toggle_group(child.clone()),
            SidebarGroupToggle::Opened
        );
        assert_eq!(state.toggle_group(leaf.clone()), SidebarGroupToggle::Opened);
        assert_eq!(state.creation_group, leaf);

        assert_eq!(
            state.toggle_group(parent.clone()),
            SidebarGroupToggle::Closed
        );
        assert!(!state.is_expanded(&parent));
        assert!(state.is_expanded(&child));
        assert!(state.is_expanded(&leaf));
        assert_eq!(state.creation_group, SidebarFilter::All);

        assert_eq!(
            state.toggle_group(parent.clone()),
            SidebarGroupToggle::Opened
        );
        assert!(state.is_expanded(&child));
        assert!(state.is_expanded(&leaf));
        assert_eq!(state.creation_group, parent);

        state.reconcile_categories(["Parent/Child/Leaf"]);
        assert!(state.is_expanded(&parent));
        assert!(state.is_expanded(&child));
        assert!(state.is_expanded(&leaf));
        state.reconcile_categories(["Elsewhere"]);
        assert!(!state.is_expanded(&parent));
        assert!(!state.is_expanded(&child));
        assert!(!state.is_expanded(&leaf));
        assert_eq!(state.creation_group, SidebarFilter::All);
    }

    #[test]
    fn persisted_sidebar_state_restores_only_existing_categories() {
        let settings = SidebarSettings {
            collapsed: true,
            width: 412.0,
            expanded: vec![
                PersistedSidebarGroup::All,
                PersistedSidebarGroup::Tag("Parent".to_owned()),
                PersistedSidebarGroup::Tag("Parent/Child".to_owned()),
                PersistedSidebarGroup::Tag("Vanished".to_owned()),
            ],
            creation_group: PersistedSidebarGroup::Tag("Parent".to_owned()),
            category_order: vec![
                "Parent".to_owned(),
                "Parent/Child".to_owned(),
                "Vanished".to_owned(),
                "Personal".to_owned(),
            ],
            note_sort: vec![
                CategoryNoteSortSettings {
                    category: FAVORITED_ORDER_KEY.to_owned(),
                    field: NoteSortField::Created,
                    direction: SortDirection::Ascending,
                },
                CategoryNoteSortSettings {
                    category: "Parent/Child".to_owned(),
                    field: NoteSortField::Modified,
                    direction: SortDirection::Descending,
                },
            ],
        };
        let state = SidebarState::from_settings(&settings, ["Parent/Child/Leaf", "Personal"]);
        assert!(state.is_expanded(&SidebarFilter::All));
        assert!(state.is_expanded(&SidebarFilter::Tag("Parent".to_owned())));
        assert!(state.is_expanded(&SidebarFilter::Tag("Parent/Child".to_owned())));
        assert!(!state.is_expanded(&SidebarFilter::Tag("Vanished".to_owned())));
        assert_eq!(
            state.creation_group,
            SidebarFilter::Tag("Parent".to_owned())
        );
        assert_eq!(
            state.to_settings(412.0),
            SidebarSettings {
                collapsed: true,
                width: 412.0,
                expanded: vec![
                    PersistedSidebarGroup::All,
                    PersistedSidebarGroup::Tag("Parent".to_owned()),
                    PersistedSidebarGroup::Tag("Parent/Child".to_owned()),
                ],
                creation_group: PersistedSidebarGroup::Tag("Parent".to_owned()),
                category_order: vec![
                    "Parent".to_owned(),
                    "Parent/Child".to_owned(),
                    "Parent/Child/Leaf".to_owned(),
                    "Personal".to_owned(),
                ],
                note_sort: vec![
                    CategoryNoteSortSettings {
                        category: "Parent/Child".to_owned(),
                        field: NoteSortField::Modified,
                        direction: SortDirection::Descending,
                    },
                    CategoryNoteSortSettings {
                        category: FAVORITED_ORDER_KEY.to_owned(),
                        field: NoteSortField::Created,
                        direction: SortDirection::Ascending,
                    },
                ],
            }
        );
    }

    #[test]
    fn category_order_reconciles_new_paths_first_and_drops_stale_paths() {
        let mut state = SidebarState::default();
        state.reconcile_categories(["Personal", "Work/Archive", "Work/Planning"]);
        assert_eq!(
            state.category_order,
            ["Personal", "Work", "Work/Archive", "Work/Planning"]
        );

        assert!(state.reorder_category("Work", "Personal", CategoryDropPosition::Before));
        assert!(state.reorder_category(
            "Work/Planning",
            "Work/Archive",
            CategoryDropPosition::Before
        ));
        assert_eq!(
            state.category_order,
            ["Work", "Work/Planning", "Work/Archive", "Personal"]
        );

        state.reconcile_categories([
            "Personal",
            "Inbox",
            "Work/Archive",
            "Work/New",
            "Work/Planning",
        ]);
        assert_eq!(
            state.category_order,
            [
                "Inbox",
                "Work",
                "Work/New",
                "Work/Planning",
                "Work/Archive",
                "Personal",
            ]
        );

        state.reconcile_categories(["Personal", "Inbox", "Work/Archive", "Work/New"]);
        assert_eq!(
            state.category_order,
            ["Inbox", "Work", "Work/New", "Work/Archive", "Personal"]
        );
        state.reconcile_categories([
            "Personal",
            "Inbox",
            "Work/Archive",
            "Work/New",
            "Work/Planning",
        ]);
        assert_eq!(
            state.category_order,
            [
                "Inbox",
                "Work",
                "Work/Planning",
                "Work/New",
                "Work/Archive",
                "Personal",
            ]
        );
    }

    #[test]
    fn category_reorder_moves_subtrees_and_rejects_cross_parent_or_self_drop() {
        let mut state = SidebarState::default();
        state.reconcile_categories(["Alpha/One", "Alpha/Two/Leaf", "Beta/One", "Gamma"]);
        assert!(state.reorder_category("Beta", "Alpha", CategoryDropPosition::Before));
        assert_eq!(
            state.category_order,
            [
                "Beta",
                "Beta/One",
                "Alpha",
                "Alpha/One",
                "Alpha/Two",
                "Alpha/Two/Leaf",
                "Gamma",
            ]
        );
        assert!(state.reorder_category("Alpha/Two", "Alpha/One", CategoryDropPosition::Before));
        assert_eq!(
            state.category_order,
            [
                "Beta",
                "Beta/One",
                "Alpha",
                "Alpha/Two",
                "Alpha/Two/Leaf",
                "Alpha/One",
                "Gamma",
            ]
        );
        let unchanged = state.category_order.clone();
        assert!(!state.reorder_category("Beta", "Alpha", CategoryDropPosition::Before));
        assert!(!state.reorder_category("Alpha/Two", "Alpha/One", CategoryDropPosition::Before));
        assert!(!state.reorder_category("Alpha/One", "Beta/One", CategoryDropPosition::Before));
        assert!(!state.reorder_category("Alpha", "Alpha", CategoryDropPosition::After));
        assert_eq!(state.category_order, unchanged);
    }

    #[test]
    fn category_drag_hit_testing_matches_visible_sidebar_geometry() {
        let rows = vec![
            SidebarRow::Group {
                filter: SidebarFilter::Favorites,
                title: tr!(Favorites),
                count: 0,
                depth: 0,
            },
            SidebarRow::Separator,
            SidebarRow::Group {
                filter: SidebarFilter::Tag("Work".to_owned()),
                title: "Work".to_owned(),
                count: 1,
                depth: 0,
            },
            SidebarRow::Note {
                parent: SidebarFilter::Tag("Work".to_owned()),
                index: 0,
                depth: 0,
            },
            SidebarRow::Group {
                filter: SidebarFilter::Tag("Personal".to_owned()),
                title: "Personal".to_owned(),
                count: 1,
                depth: 0,
            },
            SidebarRow::Group {
                filter: SidebarFilter::Tag("Parent/Child".to_owned()),
                title: "Child".to_owned(),
                count: 1,
                depth: 1,
            },
            SidebarRow::Group {
                filter: SidebarFilter::Tag("Parent/Other".to_owned()),
                title: "Other".to_owned(),
                count: 1,
                depth: 1,
            },
        ];
        let regions = category_drag_hit_regions(&rows);
        assert_eq!(regions.len(), 4);
        assert_eq!(regions[0].path, "Work");
        assert_eq!((regions[0].top, regions[0].bottom), (44.0, 78.0));
        assert_eq!(regions[1].path, "Personal");
        assert_eq!((regions[1].top, regions[1].bottom), (112.0, 146.0));

        assert_eq!(
            category_drag_source_at_point(&regions, Point::new(12.0, 50.0), 256.0),
            Some("Work".to_owned())
        );
        assert_eq!(
            category_drag_source_at_point(&regions, Point::new(12.0, 90.0), 256.0),
            None
        );
        assert_eq!(
            category_drop_target_at_point(&regions, "Work", Point::new(12.0, 113.0), 256.0,),
            Some(("Personal".to_owned(), CategoryDropPosition::Before))
        );
        assert_eq!(
            category_drop_target_at_point(&regions, "Work", Point::new(12.0, 145.0), 256.0,),
            Some(("Personal".to_owned(), CategoryDropPosition::After))
        );
        assert_eq!(
            category_drop_target_at_point(
                &regions,
                "Parent/Child",
                Point::new(12.0, regions[3].top + 1.0),
                256.0,
            ),
            Some(("Parent/Other".to_owned(), CategoryDropPosition::Before))
        );
        assert_eq!(
            category_drop_target_at_point(
                &regions,
                "Work",
                Point::new(12.0, regions[2].top + 1.0),
                256.0,
            ),
            None
        );
        assert_eq!(
            category_drop_target_at_point(
                &regions,
                "Work",
                Point::new(300.0, regions[1].top + 1.0),
                256.0,
            ),
            None
        );
    }

    #[test]
    fn note_drag_target_stays_inside_pin_partition_and_reorders_paths() {
        let paths = [
            (
                CatalogOrderItem::Note(std::path::PathBuf::from("p1.md")),
                true,
            ),
            (
                CatalogOrderItem::Note(std::path::PathBuf::from("p2.md")),
                true,
            ),
            (
                CatalogOrderItem::Note(std::path::PathBuf::from("a.md")),
                false,
            ),
            (
                CatalogOrderItem::Note(std::path::PathBuf::from("b.md")),
                false,
            ),
            (
                CatalogOrderItem::Note(std::path::PathBuf::from("c.md")),
                false,
            ),
        ];
        let a = CatalogOrderItem::Note(std::path::PathBuf::from("a.md"));
        let p2 = CatalogOrderItem::Note(std::path::PathBuf::from("p2.md"));
        let c = CatalogOrderItem::Note(std::path::PathBuf::from("c.md"));
        assert_eq!(
            note_drop_target(&paths, &a, false, 80.0),
            Some((c.clone(), CategoryDropPosition::After,))
        );
        assert_eq!(note_drop_target(&paths, &a, false, -200.0), None);
        assert_eq!(note_drop_target(&paths, &p2, true, 200.0), None);
        assert_eq!(
            reordered_catalog_items(&paths, &a, &c, CategoryDropPosition::After,).unwrap(),
            ["p1.md", "p2.md", "b.md", "c.md", "a.md"]
                .map(|path| CatalogOrderItem::Note(std::path::PathBuf::from(path)))
        );
    }

    #[test]
    fn category_notes_use_manual_order_then_persisted_automatic_date_sort() {
        let root = test_workspace("stillus-note-sort");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).unwrap();
        let fixtures = [
            (
                "Pinned.md",
                "---\ntitle: Pinned\ntags: [Work]\npinned: true\nfavorited: true\ncreated: '2020-01-01T00:00:00Z'\norder: {'Work': 0, '__favorited': 0}\n---\nPinned\n",
            ),
            (
                "Alpha.md",
                "---\ntitle: Alpha\ntags: [Work]\nfavorited: true\ncreated: '2022-01-01T00:00:00Z'\norder: {'Work': 2, '__favorited': 2}\n---\nAlpha\n",
            ),
            (
                "Beta.md",
                "---\ntitle: Beta\ntags: [Work]\nfavorited: true\ncreated: '2021-01-01T00:00:00Z'\norder: {'Work': 0, '__favorited': 0}\n---\nBeta\n",
            ),
            (
                "Charlie.md",
                "---\ntitle: Charlie\ntags: [Work]\nfavorited: true\ncreated: '2023-01-01T00:00:00Z'\n---\nCharlie\n",
            ),
        ];
        for (name, contents) in fixtures {
            fs::write(notes.join(name), contents).unwrap();
        }
        let mut model = AppModel::load(&root);
        let mut state = SidebarState::default();
        state.expanded.insert(SidebarFilter::Tag("Work".to_owned()));
        let titles = |model: &AppModel, state: &SidebarState| {
            let workspace = model.workspace.as_ref().unwrap();
            current_sidebar_rows(model, state)
                .into_iter()
                .filter_map(|row| match row {
                    SidebarRow::Note {
                        parent: SidebarFilter::Tag(category),
                        index,
                        ..
                    } if category == "Work" => Some(workspace.notes()[index].title.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            titles(&model, &state),
            ["Pinned", "Beta", "Alpha", "Charlie"]
        );

        let cleared = model.clear_category_note_order("Work");
        eprintln!(
            "NATIVE_ASSERT operation=NoteOrder success={}",
            cleared == Some(true)
        );
        assert_eq!(cleared, Some(true));
        state.set_note_sort(
            "Work".to_owned(),
            NoteSort {
                field: NoteSortField::Created,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(
            titles(&model, &state),
            ["Pinned", "Charlie", "Alpha", "Beta"]
        );
        assert!(
            model
                .workspace
                .as_ref()
                .unwrap()
                .notes()
                .iter()
                .all(|note| !note.order.contains_key("Work"))
        );

        state.expanded.insert(SidebarFilter::Favorites);
        let favorite_titles = |model: &AppModel, state: &SidebarState| {
            let workspace = model.workspace.as_ref().unwrap();
            current_sidebar_rows(model, state)
                .into_iter()
                .filter_map(|row| match row {
                    SidebarRow::Note {
                        parent: SidebarFilter::Favorites,
                        index,
                        ..
                    } => Some(workspace.notes()[index].title.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            favorite_titles(&model, &state),
            ["Pinned", "Beta", "Alpha", "Charlie"]
        );
        assert_eq!(
            model.clear_sidebar_note_order(&SidebarFilter::Favorites),
            Some(true)
        );
        state.set_note_sort(
            FAVORITED_ORDER_KEY.to_owned(),
            NoteSort {
                field: NoteSortField::Created,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(
            favorite_titles(&model, &state),
            ["Pinned", "Charlie", "Alpha", "Beta"]
        );
        assert!(
            model
                .workspace
                .as_ref()
                .unwrap()
                .notes()
                .iter()
                .all(|note| !note.order.contains_key(FAVORITED_ORDER_KEY))
        );
        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn category_drag_threshold_keeps_micro_move_as_click() {
        let origin = Point::new(20.0, 20.0);
        assert!(!category_drag_threshold_reached(
            origin,
            Point::new(22.0, 22.0)
        ));
        assert!(!category_drag_threshold_reached(
            origin,
            Point::new(23.99, 20.0)
        ));
        assert!(category_drag_threshold_reached(
            origin,
            Point::new(24.0, 20.0)
        ));
    }

    #[test]
    fn editor_metrics_fill_the_available_surface_width() {
        let root = test_workspace("stillus-app-left-aligned-editor");
        fs::create_dir_all(root.join("notes")).expect("create editor metrics workspace");

        let mut model = AppModel::load(&root);
        assert!(model.update_editor_metrics(1_200.0, 640.0));
        assert_eq!(
            model.editor_padding_x,
            EDITOR_LINE_NUMBER_MIN_WIDTH_PX + EDITOR_LINE_NUMBER_GAP_PX
        );
        let expected_content_width = 1_200.0 - model.editor_padding_x - EDITOR_PADDING_X_PX;
        assert!(
            (model.editor_content_width - expected_content_width).abs() < 0.01,
            "content width {} should fill available {}",
            model.editor_content_width,
            expected_content_width
        );
        assert_eq!(
            model.editor_columns,
            (expected_content_width / model.editor_character_width).floor() as usize
        );
        assert!(model.editor_columns > 96);

        assert!(model.update_editor_metrics(1.0, 100.0));
        assert_eq!(
            model.editor_padding_x,
            EDITOR_LINE_NUMBER_MIN_WIDTH_PX + EDITOR_LINE_NUMBER_GAP_PX
        );
        assert_eq!(model.editor_columns, 8);

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove editor metrics workspace");
    }

    #[test]
    fn line_number_gutter_grows_only_when_the_document_needs_more_digits() {
        assert_eq!(decimal_digits(0), 1);
        assert_eq!(decimal_digits(9), 1);
        assert_eq!(decimal_digits(10), 2);
        assert_eq!(decimal_digits(999), 3);
        assert_eq!(decimal_digits(1_000), 4);
        assert_eq!(
            editor_line_number_width(9, EDITOR_CHARACTER_WIDTH_PX),
            EDITOR_LINE_NUMBER_MIN_WIDTH_PX
        );
        assert!(
            editor_line_number_width(1_000, EDITOR_CHARACTER_WIDTH_PX)
                > editor_line_number_width(100, EDITOR_CHARACTER_WIDTH_PX)
        );
    }

    #[test]
    fn editor_wheel_accumulates_pixels_into_controlled_line_steps() {
        let mut remainder = 0.0;
        assert_eq!(editor_wheel_line_delta(&mut remainder, 20.0), 0);
        assert_eq!(remainder, 20.0);
        assert_eq!(editor_wheel_line_delta(&mut remainder, 20.0), 0);
        assert_eq!(remainder, 40.0);
        assert_eq!(editor_wheel_line_delta(&mut remainder, 20.0), 1);
        assert_eq!(remainder, 0.0);

        assert_eq!(editor_wheel_line_delta(&mut remainder, 60.0), 1);
        assert_eq!(editor_wheel_line_delta(&mut remainder, -60.0), -1);
        assert_eq!(remainder, 0.0);

        assert_eq!(editor_wheel_line_delta(&mut remainder, 18.0), 0);
        assert_eq!(editor_wheel_line_delta(&mut remainder, -18.0), 0);
        assert_eq!(remainder, 0.0);

        remainder = 12.0;
        assert_eq!(editor_wheel_line_delta(&mut remainder, f64::NAN), 0);
        assert_eq!(editor_wheel_line_delta(&mut remainder, f64::INFINITY), 0);
        assert_eq!(remainder, 12.0);

        remainder = 0.0;
        assert_eq!(editor_wheel_line_delta(&mut remainder, 600.0), 4);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn go_to_line_validates_one_based_input() {
        assert_eq!(parse_go_to_line(" 7 ", 12), Ok(6));
        assert_eq!(parse_go_to_line("", 12), Err(GoToLineError::Empty));
        assert_eq!(parse_go_to_line("hello", 12), Err(GoToLineError::Invalid));
        assert_eq!(
            parse_go_to_line("0", 12),
            Err(GoToLineError::OutOfRange { maximum: 12 })
        );
        assert_eq!(
            parse_go_to_line("13", 12),
            Err(GoToLineError::OutOfRange { maximum: 12 })
        );
    }

    #[test]
    fn go_to_line_moves_to_the_line_start_without_changing_note_bytes() {
        let root = test_workspace("stillus-app-go-to-line");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create go-to-line workspace");
        let note = notes.join("Lines.md");
        let body = (1..=80)
            .map(|line| {
                if line < 57 {
                    format!("line {line:02} {}", "word ".repeat(80))
                } else {
                    format!("line {line:02}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&note, &body).expect("write go-to-line note");

        let before = fs::read(&note).expect("read note before navigation");
        let mut model = AppModel::load(&root);
        model.update_editor_metrics(720.0, 240.0);
        let content_revision = model
            .workspace
            .as_ref()
            .and_then(super::WorkspaceSession::document)
            .expect("document opens")
            .content_revision();
        assert_eq!(go_to_line(&mut model, "57"), Ok(56));
        let document = model
            .workspace
            .as_ref()
            .and_then(super::WorkspaceSession::document)
            .expect("document stays open");
        assert_eq!(document.cursor_line(), Ok(56));
        assert_eq!(document.content_revision(), content_revision);
        let line = document
            .viewport(stillus_core::ViewportRequest {
                first_line: 56,
                visible_lines: 1,
                overscan_lines: 0,
            })
            .expect("target line viewport")
            .lines
            .into_iter()
            .next()
            .expect("target line");
        assert_eq!(document.selection().focus().get(), line.start.get());
        assert!(model.viewport_first_line <= 56);
        assert!(56 < model.viewport_first_line + model.editor_rows.max(1));
        let (_, caret_y) = caret_geometry(&model).expect("target caret is rendered");
        assert!(
            caret_y < EDITOR_PADDING_Y_PX + model.editor_rows.max(1) as f64 * EDITOR_LINE_HEIGHT_PX
        );
        assert_eq!(fs::read(&note).expect("read note after navigation"), before);

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove go-to-line workspace");
    }

    #[test]
    fn note_find_reveals_match_hidden_below_wrapped_visual_rows() {
        let root = test_workspace("stillus-app-find-reveal");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create find-reveal workspace");
        let note = notes.join("Find.md");
        let body = format!(
            "# Find\n{}deepneedle\nnearby context\nneedle target\n",
            "wrapped words ".repeat(80)
        );
        fs::write(&note, &body).expect("write find-reveal note");

        let before = fs::read(&note).expect("read note before find navigation");
        let mut model = AppModel::load(&root);
        model.update_editor_metrics(
            280.0,
            2.0 * EDITOR_PADDING_Y_PX + 6.0 * EDITOR_LINE_HEIGHT_PX + 1.0,
        );
        let (range, content_revision) = {
            let document = model
                .workspace
                .as_ref()
                .and_then(super::WorkspaceSession::document)
                .expect("find document opens");
            (
                document
                    .find_case_insensitive("needle target", 1)
                    .into_iter()
                    .next()
                    .expect("find match"),
                document.content_revision(),
            )
        };
        model.apply(EditorCommand::SetSelection {
            anchor: range.start().get(),
            focus: range.end().get(),
        });

        assert_eq!(model.viewport_first_line, 0);
        assert!(!editor_selection_is_fully_visible(&model, range));
        model.reveal_editor_selection(range);
        assert!(model.viewport_first_line > 0);
        assert!(editor_selection_is_fully_visible(&model, range));

        let settled = model.viewport_first_line;
        model.reveal_editor_selection(range);
        assert_eq!(model.viewport_first_line, settled);

        model.viewport_first_line = 0;
        model.viewport_first_visual_row = 0;
        let deep_range = model
            .workspace
            .as_ref()
            .and_then(super::WorkspaceSession::document)
            .expect("find document stays open")
            .find_case_insensitive("deepneedle", 1)
            .into_iter()
            .next()
            .expect("deep wrapped match");
        model.apply(EditorCommand::SetSelection {
            anchor: deep_range.start().get(),
            focus: deep_range.end().get(),
        });
        assert!(!editor_selection_is_fully_visible(&model, deep_range));
        model.reveal_editor_selection(deep_range);
        assert!(model.viewport_first_visual_row > 0);
        assert!(editor_selection_is_fully_visible(&model, deep_range));

        let document = model
            .workspace
            .as_ref()
            .and_then(super::WorkspaceSession::document)
            .expect("find document stays open");
        assert_eq!(document.selection().normalized(), deep_range);
        assert_eq!(document.content_revision(), content_revision);
        assert_eq!(
            fs::read(&note).expect("read note after find navigation"),
            before
        );

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove find-reveal workspace");
    }

    #[test]
    fn pointer_hit_testing_uses_the_first_scrolled_row_without_a_marker_reserve() {
        let root = test_workspace("stillus-app-pointer-header");
        fs::create_dir_all(root.join("notes")).expect("create pointer workspace");
        let body = (0..30)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("notes/Rows.md"), &body).expect("write pointer note");
        let mut model = AppModel::load(&root);
        let origin_x = editor_horizontal_metrics(&model).0;
        model.update_editor_metrics(
            origin_x + EDITOR_PADDING_X_PX + 10.0 * EDITOR_CHARACTER_WIDTH_PX + 1.0,
            2.0 * EDITOR_PADDING_Y_PX + 8.0 * EDITOR_LINE_HEIGHT_PX + 1.0,
        );
        model.viewport_first_line = 5;
        let offset = body.find("line 5").expect("sixth line");
        let layout = editor_layout(&model).expect("scrolled layout");
        assert_eq!(layout.geometry.rows().len(), 8);
        assert_eq!(
            layout.geometry.caret(5, offset).expect("first caret").row,
            0
        );
        assert_eq!(
            editor_command_for_point(
                &model,
                origin_x,
                EDITOR_PADDING_Y_PX + EDITOR_LINE_HEIGHT_PX / 2.0,
                false
            ),
            Some(EditorCommand::SetCaret {
                offset,
                extend: false
            })
        );
        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove pointer workspace");
    }

    #[test]
    fn line_numbers_label_only_the_first_visual_row_of_a_wrapped_line() {
        let root = test_workspace("stillus-app-line-numbers");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create line-number workspace");
        let body = std::iter::once("abcdefghijklmnop".to_owned())
            .chain((2..=12).map(|line| format!("line {line}")))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(notes.join("Lines.md"), body).expect("write line-number note");

        let mut model = AppModel::load(&root);
        let origin_x = editor_horizontal_metrics(&model).0;
        model.update_editor_metrics(
            origin_x + EDITOR_PADDING_X_PX + 8.0 * EDITOR_CHARACTER_WIDTH_PX + 1.0,
            2.0 * EDITOR_PADDING_Y_PX + 8.0 * EDITOR_LINE_HEIGHT_PX + 1.0,
        );
        let rendered = render_editor_line_numbers(&model);
        let rows = rendered.lines().collect::<Vec<_>>();
        assert_eq!(rows[0], " 1");
        assert_eq!(rows[1], "  ");
        assert_eq!(rows[2], " 2");
        assert!(!rendered.contains('⋯'));
        assert!(
            rows.last()
                .is_some_and(|row| row.trim().parse::<usize>().is_ok())
        );

        model.viewport_first_line = 5;
        let scrolled = render_editor_line_numbers(&model);
        assert_eq!(scrolled.lines().next(), Some(" 6"));
        assert!(!super::render_editor(&model).contains('⋯'));

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove line-number workspace");
    }

    #[test]
    fn rss_rows_share_sidebar_groups_and_keyboard_navigation_marks_entries_read() {
        let root = test_workspace("stillus-app-rss-sidebar");
        fs::create_dir_all(root.join("notes")).expect("create RSS workspace");
        fs::write(
            root.join("notes/Note.md"),
            "---\ntitle: Note\ntags:\n  - Work\n---\nbody\n",
        )
        .expect("write note");
        let mut model = AppModel::load(&root);
        let rss_id = model
            .application
            .test_workspace_mut()
            .expect("workspace opens")
            .create_rss(
                "https://example.test/feed",
                vec!["Work".to_owned()],
                false,
                "2025-09-01T10:00:00Z",
            )
            .expect("create RSS subscription");
        model
            .application
            .test_workspace_mut()
            .unwrap()
            .finish_rss_refresh(RssRefreshResult::Fetched {
                item_id: rss_id,
                cache: RssFeedCache {
                    entries: ["first", "second"]
                        .into_iter()
                        .map(|id| RssEntry {
                            id: id.to_owned(),
                            title: id.to_owned(),
                            author: None,
                            published: None,
                            updated: None,
                            summary: String::new(),
                            link: None,
                        })
                        .collect(),
                    fetched_at: Some("2025-09-01T10:01:00Z".to_owned()),
                    ..RssFeedCache::default()
                },
            })
            .expect("cache RSS entries");
        let mut state = SidebarState::default();
        state.expanded.insert(SidebarFilter::Tag("Work".to_owned()));
        let rows = current_sidebar_rows(&model, &state);
        assert!(rows.iter().any(|row| matches!(
            row,
            SidebarRow::Engine {
                parent: SidebarFilter::All,
                ..
            }
        )));
        assert!(rows.iter().any(|row| matches!(
            row,
            SidebarRow::Engine {
                parent: SidebarFilter::Tag(category),
                ..
            } if category == "Work"
        )));
        assert!(model.move_rss_selection(1));
        assert_eq!(model.selected_rss_entry.as_deref(), Some("first"));
        assert!(model.move_rss_selection(1));
        assert_eq!(model.selected_rss_entry.as_deref(), Some("second"));
        assert!(!model.move_rss_selection(1));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while model.workspace.as_ref().unwrap().rss_subscriptions()[0].unread != 0
            && std::time::Instant::now() < deadline
        {
            model.poll_rss();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            model.workspace.as_ref().unwrap().rss_subscriptions()[0].unread,
            0
        );

        let feed_id = model
            .workspace
            .as_ref()
            .unwrap()
            .selected_rss()
            .unwrap()
            .clone();
        let set_filter = |model: &mut AppModel, blacklist: &str| {
            model.rss_save_sequence += 1;
            let token = model.rss_save_sequence;
            let expected = model
                .workspace
                .as_ref()
                .unwrap()
                .rss_preferences(&feed_id)
                .unwrap()
                .version;
            assert!(model.rss_command(super::rss_service::Command::Preferences(
                feed_id.clone(),
                token,
                expected,
                stillus_core::RssPreferences {
                    blacklist: blacklist.into(),
                    ..Default::default()
                },
                true,
            )));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while model.rss_saves.get(feed_id.as_str()) != Some(&(token, true)) {
                assert!(std::time::Instant::now() < deadline);
                model.poll_rss();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        set_filter(&mut model, "second");
        assert_eq!(model.selected_rss_entry.as_deref(), Some("second"));
        assert!(model.move_rss_selection(-1));
        assert_eq!(model.selected_rss_entry.as_deref(), Some("first"));
        assert!(!model.move_rss_selection(1));
        set_filter(&mut model, "");
        assert!(model.move_rss_selection(1));

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove RSS workspace");
    }

    #[test]
    fn group_activation_opens_first_match_only_when_selection_is_empty() {
        for round in 1..=32 {
            group_activation_round(round);
        }
    }

    fn group_activation_round(round: usize) {
        let root = test_workspace("stillus-app-group-activation");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create group-activation workspace");
        fs::write(notes.join("A Trash.md"), "trash\n").expect("write initially selected note");
        fs::write(
            notes.join("B Work.md"),
            "---\ntags: [Work/Planning]\ntitle: B Work\n---\nwork body\n",
        )
        .expect("write first Work note");
        fs::write(
            notes.join("C Favorite.md"),
            "---\nfavorited: true\ntags: [Work/Review]\ntitle: C Favorite\n---\nfavorite body\n",
        )
        .expect("write favorite Work note");

        let mut model = AppModel::load(&root);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            eprintln!("NATIVE_DELETE_TEST round={round} deletion=1 phase=Begin");
            let deleted = model.set_deleted_selected(true);
            eprintln!("NATIVE_DELETE_TEST round={round} deletion=1 phase=End");
            eprintln!("NATIVE_ASSERT operation=DeleteNote success={deleted}");
            assert!(deleted);
            assert_eq!(
                model
                    .workspace
                    .as_ref()
                    .expect("workspace stays open")
                    .selected_note(),
                None
            );

            model.open_first_matching_note_if_unselected(&SidebarFilter::Tag("Work".to_owned()));
            let workspace = model.workspace.as_ref().expect("workspace stays open");
            let selected = workspace
                .selected_note()
                .expect("first descendant note opens for virtual parent");
            assert_eq!(workspace.notes()[selected].title, "work body");

            model.open_first_matching_note_if_unselected(&SidebarFilter::Favorites);
            let workspace = model.workspace.as_ref().expect("workspace stays open");
            let selected = workspace
                .selected_note()
                .expect("existing selection is preserved");
            assert_eq!(workspace.notes()[selected].title, "work body");

            eprintln!("NATIVE_DELETE_TEST round={round} deletion=2 phase=Begin");
            let deleted = model.set_deleted_selected(true);
            eprintln!("NATIVE_DELETE_TEST round={round} deletion=2 phase=End");
            eprintln!("NATIVE_ASSERT operation=DeleteNote success={deleted}");
            assert!(deleted);
            model.open_first_matching_note_if_unselected(&SidebarFilter::Tag("Gone".to_owned()));
            assert_eq!(
                model
                    .workspace
                    .as_ref()
                    .expect("workspace stays open")
                    .selected_note(),
                None
            );
        }));

        model.shutdown_search_worker();
        drop(model);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
        fs::remove_dir_all(root).expect("remove group-activation workspace");
    }

    #[test]
    fn wrapped_rows_map_pointer_columns_caret_and_selection_back_to_the_line() {
        let root = test_workspace("stillus-app-wrapped-rows");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create wrapped-rows test workspace");
        fs::write(notes.join("Wrap.md"), "alpha bravo charlie\nsecond\n")
            .expect("write wrapped-rows note");

        let mut model = AppModel::load(&root);
        let origin_x = editor_horizontal_metrics(&model).0;
        assert!(model.update_editor_metrics(
            origin_x + EDITOR_PADDING_X_PX + 10.0 * EDITOR_CHARACTER_WIDTH_PX + 1.0,
            2.0 * EDITOR_PADDING_Y_PX + 5.0 * EDITOR_LINE_HEIGHT_PX + 1.0,
        ));
        assert_eq!((model.editor_columns, model.editor_rows), (10, 5));

        let layout = editor_layout(&model).expect("shape wrapped rows");
        let charlie_start = "alpha bravo ".len();
        let charlie_after_c = "alpha bravo c".len();
        let charlie_start_caret = layout
            .geometry
            .caret(0, charlie_start)
            .expect("charlie start caret");
        let charlie_after_c_caret = layout
            .geometry
            .caret(0, charlie_after_c)
            .expect("charlie glyph caret");
        let second_row_y =
            EDITOR_PADDING_Y_PX + (charlie_after_c_caret.row as f64 + 0.5) * EDITOR_LINE_HEIGHT_PX;
        let x = charlie_after_c_caret.x;
        assert_eq!(
            editor_command_for_point(&model, x, second_row_y, false),
            Some(EditorCommand::SetCaret {
                offset: charlie_after_c,
                extend: false,
            })
        );
        assert_eq!(
            editor_word_command_for_point(
                &model,
                (charlie_start_caret.x + charlie_after_c_caret.x) / 2.0,
                second_row_y,
            ),
            Some(EditorCommand::SetSelection {
                anchor: charlie_start,
                focus: "alpha bravo charlie".len(),
            })
        );
        let second_line_start = "alpha bravo charlie\n".len();
        let second_line_caret = layout
            .geometry
            .caret(1, second_line_start)
            .expect("second document line caret");
        let third_row_y =
            EDITOR_PADDING_Y_PX + (second_line_caret.row as f64 + 0.5) * EDITOR_LINE_HEIGHT_PX;
        assert_eq!(
            editor_command_for_point(&model, origin_x, third_row_y, false),
            Some(EditorCommand::SetCaret {
                offset: second_line_start,
                extend: false,
            })
        );

        model.apply(EditorCommand::SetCaret {
            offset: charlie_after_c,
            extend: false,
        });
        let (caret_x, caret_y) = caret_geometry(&model).expect("caret inside wrapped row");
        assert!((caret_x - charlie_after_c_caret.x).abs() < 0.01);
        let expected_y = EDITOR_PADDING_Y_PX
            + charlie_after_c_caret.row as f64 * EDITOR_LINE_HEIGHT_PX
            + (EDITOR_LINE_HEIGHT_PX - super::EDITOR_CARET_HEIGHT_PX) / 2.0;
        assert!((caret_y - expected_y).abs() < 0.01);

        // Dragging to the same focus is a no-op; dragging elsewhere extends.
        assert_eq!(editor_drag_command_for_point(&model, x, second_row_y), None);
        let drag = editor_drag_command_for_point(&model, origin_x, EDITOR_PADDING_Y_PX)
            .expect("drag to the first row extends the selection");
        assert_eq!(
            drag,
            EditorCommand::SetCaret {
                offset: 0,
                extend: true,
            }
        );
        model.apply(drag);
        let rects = editor_selection_rects(&model);
        assert!(rects.len() >= 2);
        assert_eq!(rects[0].x, origin_x);
        assert!(rects.iter().all(|rect| rect.width > 0.0));

        // Scrolling never hides the last rows when the document fits.
        model.scroll_lines(10);
        assert_eq!(model.viewport_first_line, 0);

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove wrapped-rows test workspace");
    }

    #[test]
    fn wrapped_documents_can_scroll_until_their_final_line_is_visible() {
        let root = test_workspace("stillus-app-wrapped-scroll");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create wrapped-scroll test workspace");
        let body = (0..40)
            .map(|index| format!("line-{index:03} {}", "word ".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(notes.join("Wrap.md"), format!("{body}\n")).expect("write wrapped-scroll note");

        let mut model = AppModel::load(&root);
        let origin_x = editor_horizontal_metrics(&model).0;
        model.update_editor_metrics(
            origin_x + EDITOR_PADDING_X_PX + 20.0 * EDITOR_CHARACTER_WIDTH_PX + 1.0,
            2.0 * EDITOR_PADDING_Y_PX + 6.0 * EDITOR_LINE_HEIGHT_PX + 1.0,
        );
        let total = model
            .workspace
            .as_ref()
            .and_then(super::WorkspaceSession::document)
            .expect("open document")
            .line_count();
        // 40 written lines plus the empty line after the trailing newline.
        assert_eq!(total, 41);

        assert!(!model.scroll_editor_wheel(20.0));
        assert!(!model.scroll_editor_wheel(20.0));
        assert!(model.scroll_editor_wheel(20.0));
        assert_eq!(model.viewport_first_line, 1);
        assert!(model.scroll_editor_wheel(-60.0));
        assert_eq!(model.viewport_first_line, 0);

        model.scroll_lines(10_000);
        assert_eq!(model.viewport_first_line, model.max_viewport_first_line());
        let settled = model.viewport_first_line;
        model.scroll_lines(10_000);
        assert_eq!(model.viewport_first_line, settled);

        // The final line must sit inside the visible rows, not below them.
        let layout = editor_layout(&model).expect("layout at the end of the document");
        let top = layout
            .geometry
            .rows()
            .iter()
            .position(|row| row.line_index == settled && row.start == 0)
            .expect("viewport top row");
        let final_line = layout
            .geometry
            .rows()
            .iter()
            .position(|row| row.line_index == total - 1)
            .expect("final line row");
        assert!(
            final_line - top < model.editor_rows,
            "final line renders {} rows below a {}-row viewport",
            final_line - top,
            model.editor_rows
        );

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove wrapped-scroll test workspace");
    }

    #[test]
    fn double_click_hit_testing_selects_the_word_under_the_glyph() {
        let root = test_workspace("stillus-app-word-selection");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create word-selection test workspace");
        fs::write(notes.join("Word.md"), "alpha bravo charlie\n")
            .expect("write word-selection note");

        let mut model = AppModel::load(&root);
        let layout = editor_layout(&model).expect("shape word-selection note");
        let bravo_start = "alpha ".len();
        let bravo_end = "alpha bravo".len();
        let bravo_left = layout.geometry.caret(0, bravo_start).expect("bravo start");
        let bravo_right = layout.geometry.caret(0, bravo_end).expect("bravo end");
        let x = (bravo_left.x + bravo_right.x) / 2.0;
        let command = editor_word_command_for_point(&model, x, EDITOR_PADDING_Y_PX)
            .expect("word selection command");
        assert_eq!(
            command,
            EditorCommand::SetSelection {
                anchor: "alpha ".len(),
                focus: "alpha bravo".len(),
            }
        );
        model.apply(command);
        assert_eq!(
            editor_menu_state(&model, false),
            EditorMenuState {
                can_cut_or_copy: true,
                can_paste: false,
            }
        );
        assert_eq!(
            editor_menu_state(&model, true),
            EditorMenuState {
                can_cut_or_copy: true,
                can_paste: true,
            }
        );
        let rects = editor_selection_rects(&model);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].x - bravo_left.x).abs() < 0.01);
        assert!((rects[0].width - (bravo_right.x - bravo_left.x)).abs() < 0.01);

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove word-selection test workspace");
    }

    #[test]
    fn editor_font_probe_resolves_an_installed_monospace_family_and_measures_it() {
        use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};

        let font = probe_editor_font();
        assert!(
            font.family == super::EDITOR_FALLBACK_FONT_FAMILY
                || super::EDITOR_FONT_CANDIDATES.contains(&font.family.as_str()),
            "unexpected editor family {}",
            font.family
        );
        assert!(font.character_width.is_finite() && font.character_width > 0.0);

        // The measured advance must describe the glyphs Floem really paints:
        // narrow and wide letters share one width in a monospace face.
        let families = [FamilyOwned::parse_list(&font.family)
            .next()
            .expect("resolved family parses")];
        let width_of = |sample: &str| {
            let mut layout = TextLayout::new();
            layout.set_text(
                sample,
                AttrsList::new(
                    Attrs::new()
                        .family(&families)
                        .font_size(super::EDITOR_FONT_SIZE_PX as f32),
                ),
            );
            layout.size().width
        };
        let narrow = width_of("iiiiiiiiii");
        let wide = width_of("WWWWWWWWWW");
        assert!(
            (narrow - wide).abs() < 0.5,
            "editor family {} is not monospace: {narrow} vs {wide}",
            font.family
        );
        assert!(
            (wide / 10.0 - font.character_width).abs() < 0.05,
            "measured width {} does not match layout {}",
            font.character_width,
            wide / 10.0
        );
    }

    #[test]
    fn app_model_restores_selected_note_by_path_and_falls_back_when_stale() {
        let root = test_workspace("stillus-app-settings-selection");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create settings selection workspace");
        let first = notes.join("A First.md");
        let second = notes.join("B Second.md");
        fs::write(&first, "A First\n").expect("write first note");
        fs::write(&second, "B Second\n").expect("write second note");

        let mut restored = AppModel::load_restoring(&root, Some(&second));
        let workspace = restored.workspace.as_ref().expect("workspace opens");
        let selected = workspace.selected_note().expect("selected note restores");
        assert_eq!(workspace.notes()[selected].path, second);
        restored.shutdown_search_worker();

        let mut stale = AppModel::load_restoring(&root, Some(&notes.join("Missing.md")));
        let workspace = stale.workspace.as_ref().expect("workspace opens");
        let selected = workspace.selected_note().expect("default note opens");
        assert_eq!(workspace.notes()[selected].path, first);
        stale.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove settings selection workspace");
    }

    #[test]
    fn canonical_and_native_paths_restore_notes_and_external_selection() {
        let root = test_workspace("stillus-paths 日本語");
        fs::create_dir_all(root.join("notes")).unwrap();
        let note = root.join("notes/Selected 日本語.md");
        let external = root.join("External 日本語.txt");
        fs::write(root.join("notes/A.md"), "# A\n").unwrap();
        fs::write(&note, "# Selected\n").unwrap();
        fs::write(&external, "external\n").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let canonical_note = note.canonicalize().unwrap();
        let canonical_external = external.canonicalize().unwrap();
        let persisted = [PersistedExternalFile {
            engine_id: "markdown".to_owned(),
            absolute_path: external.display().to_string(),
        }];
        for workspace_root in [&root, &canonical_root] {
            for selected in [&note, &canonical_note] {
                let mut model = AppModel::load_restoring(workspace_root, Some(selected));
                let workspace = model.workspace.as_ref().unwrap();
                let index = workspace.selected_note().unwrap();
                assert_eq!(
                    workspace.notes()[index].path.canonicalize().unwrap(),
                    canonical_note
                );
                model.shutdown_search_worker();
            }
            for selected in [&external, &canonical_external] {
                let mut model = AppModel::load_restoring_state(
                    workspace_root,
                    None,
                    &persisted,
                    Some(selected),
                    None,
                );
                let workspace = model.workspace.as_ref().unwrap();
                stillus_platform::diagnostics::path_comparison(
                    stillus_platform::diagnostics::PathOperation::ExternalSelection,
                    selected,
                    &workspace.external_files()[0].path,
                );
                assert!(matches!(
                    workspace.selected_target(),
                    Some(DocumentTarget::ExternalFile { .. })
                ));
                assert_eq!(workspace.external_files().len(), 1);
                model.shutdown_search_worker();
            }
        }
        assert_eq!(fs::read_to_string(&note).unwrap(), "# Selected\n");
        assert_eq!(fs::read_to_string(&external).unwrap(), "external\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn note_mutations_with_search_worker_keep_metadata_and_search_consistent() {
        let root = test_workspace("stillus-search-mutations");
        fs::create_dir_all(root.join("notes")).unwrap();
        let note = root.join("notes/Selected.md");
        fs::write(
            &note,
            "---\ntags: [Work]\norder: {Work: 1}\nfuture: keep\n---\n# searchmutationmarker\n",
        )
        .unwrap();
        let mut model = AppModel::load(&root);
        assert_eq!(
            search_results_for(&mut model, "searchmutationmarker").len(),
            1
        );
        let cleared = model.clear_category_note_order("Work");
        eprintln!(
            "NATIVE_ASSERT operation=NoteOrder success={}",
            cleared == Some(true)
        );
        assert_eq!(cleared, Some(true));
        for _ in 0..4 {
            model.request_search_reconcile();
            let deleted = model.set_deleted_selected(true);
            eprintln!("NATIVE_ASSERT operation=DeleteNote success={deleted}");
            assert!(deleted);
            model.open_first_matching_note_if_unselected(&SidebarFilter::Trash);
            assert!(model.set_deleted_selected(false));
            model.open_first_matching_note_if_unselected(&SidebarFilter::All);
        }
        model.request_search_reconcile();
        assert_eq!(
            search_results_for(&mut model, "searchmutationmarker").len(),
            1
        );
        model.shutdown_search_worker();
        let bytes = fs::read_to_string(&note).unwrap();
        assert!(bytes.contains("future: keep\n"));
        assert!(bytes.ends_with("# searchmutationmarker\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_settings_restore_selection_unavailable_rows_and_clean_close() {
        let root = test_workspace("stillus-app-external-selection");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create external selection workspace");
        fs::write(notes.join("Fallback.md"), "# Fallback\n").expect("write fallback note");
        let external = root.join("External.txt");
        let missing = root.join("Missing.md");
        fs::write(&external, "external\n").expect("write external file");
        let persisted = [
            PersistedExternalFile {
                engine_id: "markdown".to_owned(),
                absolute_path: external.display().to_string(),
            },
            PersistedExternalFile {
                engine_id: "markdown".to_owned(),
                absolute_path: missing.display().to_string(),
            },
        ];

        let mut model =
            AppModel::load_restoring_state(&root, None, &persisted, Some(external.as_path()), None);
        let workspace = model.workspace.as_ref().expect("workspace opens");
        assert_eq!(workspace.external_files().len(), 2);
        stillus_platform::diagnostics::path_comparison(
            stillus_platform::diagnostics::PathOperation::ExternalSelection,
            &external,
            &workspace.external_files()[0].path,
        );
        assert!(matches!(
            workspace.external_files()[1].availability,
            stillus_core::ItemAvailability::Unavailable(_)
        ));
        assert!(matches!(
            workspace.selected_target(),
            Some(DocumentTarget::ExternalFile { .. })
        ));
        let rows = current_sidebar_rows(&model, &SidebarState::default());
        assert_eq!(rows[0], SidebarRow::ExternalGroup { count: 2 });
        assert_eq!(rows[1], SidebarRow::ExternalFile { index: 0 });
        assert_eq!(rows[2], SidebarRow::ExternalFile { index: 1 });
        assert_eq!(rows[3], SidebarRow::Separator);

        let target = workspace.selected_target().expect("external selected");
        assert!(model.close_external_target(target));
        assert!(matches!(
            model
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_target),
            Some(DocumentTarget::WorkspaceNote(_))
        ));
        let remaining = model.workspace.as_ref().unwrap().external_files()[0].clone();
        assert!(model.close_external_target(DocumentTarget::ExternalFile {
            engine_id: remaining.engine_id,
            item_id: remaining.item_id,
        }));
        assert!(
            !current_sidebar_rows(&model, &SidebarState::default())
                .iter()
                .any(|row| matches!(row, SidebarRow::ExternalGroup { .. }))
        );

        model.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove external selection workspace");
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn note_creation_waits_for_an_active_autosave_and_then_focuses_the_editor() {
        let root = test_workspace("stillus-app-pending-note-creation");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create pending-note workspace");
        fs::write(notes.join("Existing.md"), "# Existing\n").expect("write existing note");

        let mut model = AppModel::load(&root);
        model.apply(EditorCommand::Insert("dirty ".to_owned()));
        assert!(!model.request_note_creation(SidebarFilter::All));
        assert_eq!(
            model.pending_note_creation.as_ref().map(|(scope, _)| scope),
            Some(&SidebarFilter::All)
        );
        assert!(!notes.join("New note.md").exists());

        finish_pending_persistence(&mut model);
        assert!(model.retry_pending_note_creation());
        assert!(notes.join("New note.md").is_file());
        assert!(model.pending_note_creation.is_none());
        assert!(model.note_creation_focus_pending);

        model.shutdown_search_worker();
        fs::remove_dir_all(root).expect("remove pending-note workspace");
    }

    #[test]
    fn triple_click_hit_testing_selects_the_whole_line_with_its_break() {
        let root = test_workspace("stillus-app-line-selection");
        let notes = root.join("notes");
        fs::create_dir_all(&notes).expect("create line-selection test workspace");
        fs::write(notes.join("Line.md"), "alpha bravo\nsecond\n\nlast")
            .expect("write line-selection note");

        let mut model = AppModel::load(&root);
        let origin_x = editor_horizontal_metrics(&model).0;
        let layout = editor_layout(&model).expect("shape line-selection note");
        let click_x = layout.geometry.caret(0, 9).expect("first-line caret").x;
        let row_y = |row: f64| EDITOR_PADDING_Y_PX + row * EDITOR_LINE_HEIGHT_PX + 1.0;
        let pointer = |count: u8, row: f64| PointerInputEvent {
            pos: floem::kurbo::Point::new(click_x, row_y(row)),
            button: PointerButton::Primary,
            modifiers: floem::keyboard::Modifiers::empty(),
            count,
        };

        // Click count selects caret, word or line; Floem wraps after four.
        // A single click snaps the caret to the nearest glyph boundary.
        assert_eq!(
            editor_command_for_pointer(&model, &pointer(1, 0.0)),
            Some(EditorCommand::SetCaret {
                offset: 9,
                extend: false,
            })
        );
        assert_eq!(
            editor_command_for_pointer(&model, &pointer(2, 0.0)),
            Some(EditorCommand::SetSelection {
                anchor: "alpha ".len(),
                focus: "alpha bravo".len(),
            })
        );
        let whole_first_line = EditorCommand::SetSelection {
            anchor: 0,
            focus: "alpha bravo\n".len(),
        };
        assert_eq!(
            editor_command_for_pointer(&model, &pointer(3, 0.0)),
            Some(whole_first_line.clone())
        );
        assert_eq!(
            editor_command_for_pointer(&model, &pointer(4, 0.0)),
            Some(whole_first_line)
        );

        // The second line includes its break; the empty third line selects
        // only the break; the final line without a break ends at the document.
        assert_eq!(
            editor_line_command_for_point(&model, row_y(1.0)),
            Some(EditorCommand::SetSelection {
                anchor: "alpha bravo\n".len(),
                focus: "alpha bravo\nsecond\n".len(),
            })
        );
        assert_eq!(
            editor_line_command_for_point(&model, row_y(2.0)),
            Some(EditorCommand::SetSelection {
                anchor: "alpha bravo\nsecond\n".len(),
                focus: "alpha bravo\nsecond\n\n".len(),
            })
        );
        assert_eq!(
            editor_line_command_for_point(&model, row_y(3.0)),
            Some(EditorCommand::SetSelection {
                anchor: "alpha bravo\nsecond\n\n".len(),
                focus: "alpha bravo\nsecond\n\nlast".len(),
            })
        );

        // A selected line paints one full-width rect and no stray rect on the
        // following line; a selected empty line still paints a marker.
        model.apply(
            editor_line_command_for_point(&model, row_y(1.0)).expect("second line selection"),
        );
        let rects = editor_selection_rects(&model);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].x - origin_x).abs() < 0.01);
        let second_start = "alpha bravo\n".len();
        let second_end = "alpha bravo\nsecond".len();
        let second_left = layout
            .geometry
            .caret(1, second_start)
            .expect("second line start");
        let second_right = layout
            .geometry
            .caret(1, second_end)
            .expect("second line end");
        assert!((rects[0].width - (second_right.x - second_left.x)).abs() < 0.01);
        model.apply(
            editor_line_command_for_point(&model, row_y(2.0)).expect("empty line selection"),
        );
        let rects = editor_selection_rects(&model);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].x - origin_x).abs() < 0.01);
        assert_eq!(rects[0].width, EDITOR_CHARACTER_WIDTH_PX / 2.0);
        assert!(
            (rects[0].y - (EDITOR_PADDING_Y_PX + 2.0 * EDITOR_LINE_HEIGHT_PX + 1.2)).abs() < 0.01
        );

        model.shutdown_search_worker();
        drop(model);
        fs::remove_dir_all(root).expect("remove line-selection test workspace");
    }
}
