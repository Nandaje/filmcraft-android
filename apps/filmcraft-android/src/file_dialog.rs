//! Touch-friendly Android file/folder browser modal + Storage Access Framework (SAF) system picker
//! bridge for importing and exporting video, audio, still images, captions, LUTs, interchange
//! timelines, and `.fcproj` projects.
//!
//! Why a dual-mode picker?
//! 1. **In-app Touch File & Folder Browser (`egui` Modal)**: Works immediately inside the
//!    hardware-accelerated `wgpu` surface, providing one-tap bookmarks for Android `Movies`,
//!    `Camera (DCIM)`, `Downloads`, `Pictures`, `Music`, `Documents`, `Shared Storage`,
//!    `App External`, `App Exports`, and `App Internal`, multi-file selection for media import,
//!    extension preset chips, folder creation, and permission status indicators.
//! 2. **Android Storage Access Framework (SAF) System Picker**: Invokes the native Android
//!    `ACTION_OPEN_DOCUMENT` / `ACTION_CREATE_DOCUMENT` / `ACTION_OPEN_DOCUMENT_TREE` intent via
//!    JNI (`MainActivity.kt`) so the user can import from or export to any location on the device,
//!    SD card, or cloud provider even without granting broad storage permissions.
//!
//! Every [`filmcraft_ui_egui::native_dialogs::FileDialog`] request from `FilmcraftApp` (`Import`,
//! `Open Project`, `Save`, `Save As`, `Export Media`, `Export EDL/XML/FCPXML/OTIO/ALE/CSV`,
//! `Link Media`, `Attach Proxies`, `Export Frame` folder, LUT browse, etc.) is wired through a
//! non-blocking `mpsc::Receiver<Vec<String>>` channel so the UI loop never blocks waiting on touch
//! or SAF input.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use filmcraft_ui_egui::native_dialogs::FileDialog;

use crate::permissions::{self, PermissionState};

/// Supported video extensions for import & export.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mxf", "ts", "m2ts", "mts", "mpg", "mpeg", "dng",
];

/// Supported audio extensions for import & export.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "aif", "aiff", "flac", "mp3", "aac", "m4a", "ogg", "oga", "opus", "ac3",
];

/// Supported still-image extensions for import & export.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "tga", "dpx", "exr",
];

/// Supported caption, LUT, project, and interchange extensions.
pub const AUX_EXTENSIONS: &[&str] = &[
    "fcproj", "srt", "vtt", "scc", "edl", "xml", "fcpxml", "otio", "ale", "csv", "cube", "3dl", "json",
];

/// Kind of file dialog currently open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogKind {
    /// Open one or more files.
    Open {
        multi: bool,
    },
    /// Save or export a file with a suggested name.
    Save,
    /// Pick a directory (e.g., Export destination folder, Media Browser, Proxy folder).
    PickFolder,
}

/// Unsolicited outcome from an external Android Intent or SAF callback when no channel waiter is
/// active (e.g., opening a `.fcproj` or `.mp4` from the Android Files app).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Open or import the selected file paths.
    OpenFiles(Vec<PathBuf>),
    /// Save the active project to the selected path.
    SaveProject(PathBuf),
}

/// Directory entry cached for rendering in the touch file browser.
#[derive(Clone, Debug)]
struct DirEntryRow {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size_bytes: u64,
}

/// Active state of the touch file browser modal.
struct ActiveDialog {
    title: String,
    kind: DialogKind,
    current_dir: PathBuf,
    file_name_input: String,
    /// `(label, extensions)` filter pairs.
    filters: Vec<(String, Vec<String>)>,
    active_filter_idx: usize,
    entries: Vec<DirEntryRow>,
    selected_paths: Vec<PathBuf>,
    error_banner: Option<String>,
    new_folder_name: Option<String>,
    /// Channel back to `FilmcraftApp::poll_native_dialog`, if opened by `FilmcraftApp`.
    reply: Option<Sender<Vec<String>>>,
}

impl ActiveDialog {
    fn allowed_extensions(&self) -> &[String] {
        self.filters
            .get(self.active_filter_idx)
            .map_or(&[], |(_, exts)| exts.as_slice())
    }

    fn refresh_entries(&mut self) {
        self.entries.clear();
        self.error_banner = None;

        let allowed = self.allowed_extensions().to_vec();
        let folder_only = self.kind == DialogKind::PickFolder;

        match std::fs::read_dir(&self.current_dir) {
            Ok(read_dir) => {
                let mut rows = Vec::new();
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let meta = entry.metadata().ok();
                    let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir) || path.is_dir();
                    let size_bytes = meta.as_ref().map_or(0, std::fs::Metadata::len);

                    if !is_dir {
                        if folder_only {
                            continue;
                        }
                        if !matches_extension_filter(&path, &allowed) {
                            continue;
                        }
                    }

                    rows.push(DirEntryRow { name, path, is_dir, size_bytes });
                }
                rows.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                });
                self.entries = rows;
            }
            Err(err) => {
                self.error_banner = Some(format!(
                    "Cannot list '{}': {} (grant storage permission or use the System File Picker below)",
                    self.current_dir.display(),
                    err
                ));
            }
        }
    }
}

/// Match a file path against a list of extensions (ignoring bracket glob patterns added on Linux).
#[must_use]
pub fn matches_extension_filter(path: &Path, allowed: &[String]) -> bool {
    if allowed.is_empty() || allowed.iter().any(|e| e == "*" || e.is_empty()) {
        return true;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    allowed
        .iter()
        .filter(|f| !f.contains('['))
        .any(|f| f.eq_ignore_ascii_case(ext))
}

/// Platform callbacks registered by `android_entry` to invoke JNI methods on `MainActivity`.
pub struct PlatformBridge {
    /// Request Android runtime storage/media permissions (`READ_MEDIA_VIDEO`, `READ_MEDIA_AUDIO`,
    /// `READ_MEDIA_IMAGES`, `READ_EXTERNAL_STORAGE`, `WRITE_EXTERNAL_STORAGE`,
    /// `MANAGE_EXTERNAL_STORAGE`).
    pub request_permissions: Box<dyn Fn() -> bool + Send + Sync>,
    /// Request Android `RECORD_AUDIO` permission for voice-over recording.
    pub request_audio_permission: Box<dyn Fn() -> bool + Send + Sync>,
    /// Launch Android SAF `ACTION_OPEN_DOCUMENT` picker.
    pub launch_saf_open: Box<dyn Fn(&[String], bool) -> bool + Send + Sync>,
    /// Launch Android SAF `ACTION_CREATE_DOCUMENT` picker with suggested filename and MIME type.
    pub launch_saf_save: Box<dyn Fn(&str, &str) -> bool + Send + Sync>,
    /// Launch Android SAF `ACTION_OPEN_DOCUMENT_TREE` folder picker.
    pub launch_saf_folder: Box<dyn Fn() -> bool + Send + Sync>,
    /// Notify Android `MediaScannerConnection` after writing an exported file.
    pub scan_media_file: Box<dyn Fn(&Path) + Send + Sync>,
}

#[derive(Default)]
struct DialogManager {
    active: Option<ActiveDialog>,
    /// Reply channel kept alive when the user switches from the touch modal to the Android SAF
    /// system picker so the SAF callback still fulfills `FilmcraftApp`'s `PendingDialog`.
    saf_reply: Option<Sender<Vec<String>>>,
    outcomes: Vec<DialogOutcome>,
    bridge: Option<PlatformBridge>,
}

fn manager() -> &'static Mutex<DialogManager> {
    static MANAGER: OnceLock<Mutex<DialogManager>> = OnceLock::new();
    MANAGER.get_or_init(|| Mutex::new(DialogManager::default()))
}

/// Install the JNI platform bridge at Android startup.
pub fn set_platform_bridge(bridge: PlatformBridge) {
    if let Ok(mut guard) = manager().lock() {
        guard.bridge = Some(bridge);
    }
}

/// Trigger Android runtime storage/media permission prompt through the platform bridge.
pub fn request_platform_storage_permissions() -> bool {
    let bridge_res = match manager().lock() {
        Ok(guard) => guard.bridge.as_ref().map(|b| (b.request_permissions)()),
        Err(poisoned) => poisoned.into_inner().bridge.as_ref().map(|b| (b.request_permissions)()),
    };
    if let Some(granted_immediately) = bridge_res {
        if granted_immediately {
            permissions::set_storage_permission_state(PermissionState::Granted);
            return true;
        }
        permissions::set_storage_permission_state(PermissionState::Requested);
    }
    false
}

/// Trigger Android `RECORD_AUDIO` permission prompt through the platform bridge.
pub fn request_platform_audio_permission() -> bool {
    let bridge_res = match manager().lock() {
        Ok(guard) => guard.bridge.as_ref().map(|b| (b.request_audio_permission)()),
        Err(poisoned) => poisoned.into_inner().bridge.as_ref().map(|b| (b.request_audio_permission)()),
    };
    if let Some(granted_immediately) = bridge_res {
        if granted_immediately {
            permissions::set_audio_record_permission_state(PermissionState::Granted);
            return true;
        }
        permissions::set_audio_record_permission_state(PermissionState::Requested);
    }
    false
}

/// Ask Android's `MediaScannerConnection` to index a newly exported file so it appears in Gallery,
/// Movies, Music, and the Files app immediately.
pub fn notify_media_scanner(path: &Path) {
    if let Ok(guard) = manager().lock()
        && let Some(bridge) = &guard.bridge
    {
        (bridge.scan_media_file)(path);
    }
}

/// Open a [`FileDialog`] requested by [`filmcraft_ui_egui::FilmcraftApp`] and attach `reply` so the
/// chosen path(s) are sent directly to `FilmcraftApp::poll_native_dialog`.
pub fn open_native_dialog(dialog: FileDialog, reply: Sender<Vec<String>>) {
    permissions::ensure_storage_permission();
    let dirs = permissions::current_dirs();

    let (title, kind, initial_dir, default_name, raw_filters) = match dialog {
        FileDialog::OpenFiles { exts } => (
            "Import Media / Files".to_string(),
            DialogKind::Open { multi: true },
            dirs.default_open_dir(),
            String::new(),
            if exts.is_empty() {
                Vec::new()
            } else {
                vec![("Media".to_string(), exts)]
            },
        ),
        FileDialog::OpenFile { filter, exts } => (
            format!("Open {filter}"),
            DialogKind::Open { multi: false },
            dirs.default_open_dir(),
            String::new(),
            if exts.is_empty() {
                Vec::new()
            } else {
                vec![(filter, exts)]
            },
        ),
        FileDialog::Relink { exts, .. } => (
            "Locate / Relink Media".to_string(),
            DialogKind::Open { multi: false },
            dirs.default_open_dir(),
            String::new(),
            if exts.is_empty() {
                Vec::new()
            } else {
                vec![("Media".to_string(), exts)]
            },
        ),
        FileDialog::OpenProject => (
            "Open FilmCraft Project".to_string(),
            DialogKind::Open { multi: false },
            dirs.default_open_dir(),
            String::new(),
            vec![("FilmCraft Project (.fcproj)".to_string(), vec!["fcproj".to_string()])],
        ),
        FileDialog::SaveProject { name } => (
            "Save FilmCraft Project".to_string(),
            DialogKind::Save,
            dirs.default_save_dir(),
            name,
            vec![("FilmCraft Project (.fcproj)".to_string(), vec!["fcproj".to_string()])],
        ),
        FileDialog::SaveAs { filter, exts, name } => {
            let path = Path::new(&name);
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty() && permissions::is_dir_writable(d))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dirs.default_save_dir());
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(name.as_str())
                .to_string();
            (
                format!("Export / Save {filter}"),
                DialogKind::Save,
                dir,
                file_name,
                if exts.is_empty() {
                    Vec::new()
                } else {
                    vec![(filter, exts)]
                },
            )
        }
        FileDialog::Folder { at } => {
            let dir = at
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| dirs.default_save_dir());
            (
                "Select Folder".to_string(),
                DialogKind::PickFolder,
                dir,
                String::new(),
                Vec::new(),
            )
        }
    };

    let filters = normalize_filters(raw_filters, &kind);
    let mut active = ActiveDialog {
        title,
        kind,
        current_dir: initial_dir,
        file_name_input: default_name,
        filters,
        active_filter_idx: 0,
        entries: Vec::new(),
        selected_paths: Vec::new(),
        error_banner: None,
        new_folder_name: None,
        reply: Some(reply),
    };
    active.refresh_entries();

    if let Ok(mut guard) = manager().lock() {
        guard.active = Some(active);
    }
}

fn normalize_filters(raw: Vec<(String, Vec<String>)>, kind: &DialogKind) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (label, exts) in raw {
        let cleaned: Vec<String> = exts
            .into_iter()
            .filter(|e| !e.contains('[') && !e.is_empty() && e != "*")
            .collect();
        if !cleaned.is_empty() {
            out.push((label, cleaned));
        }
    }
    if matches!(kind, DialogKind::Open { .. }) {
        if out.is_empty() {
            let mut all_media = Vec::new();
            for group in [VIDEO_EXTENSIONS, AUDIO_EXTENSIONS, IMAGE_EXTENSIONS, AUX_EXTENSIONS] {
                all_media.extend(group.iter().map(|s| (*s).to_string()));
            }
            out.push(("All Supported Media".to_string(), all_media));
            out.push(("Video".to_string(), VIDEO_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()));
            out.push(("Audio".to_string(), AUDIO_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()));
            out.push(("Images".to_string(), IMAGE_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()));
            out.push(("Projects (.fcproj)".to_string(), vec!["fcproj".to_string()]));
        }
        out.push(("All Files (*.*)".to_string(), Vec::new()));
    } else if matches!(kind, DialogKind::Save) && out.is_empty() {
        out.push(("FilmCraft Project (.fcproj)".to_string(), vec!["fcproj".to_string()]));
        out.push(("MP4 Video (.mp4)".to_string(), vec!["mp4".to_string()]));
        out.push(("QuickTime (.mov)".to_string(), vec!["mov".to_string()]));
        out.push(("WAV Audio (.wav)".to_string(), vec!["wav".to_string()]));
    }
    out
}

/// Push a completed unsolicited dialog outcome (e.g., from an external Android `ACTION_VIEW`
/// intent). If an active dialog or SAF reply channel is waiting, sends the path(s) to that channel
/// first.
pub fn push_outcome(outcome: DialogOutcome) {
    if let Ok(mut guard) = manager().lock() {
        let reply = guard
            .saf_reply
            .take()
            .or_else(|| guard.active.take().and_then(|mut d| d.reply.take()));
        if let Some(tx) = reply {
            let paths = match outcome {
                DialogOutcome::OpenFiles(v) => v.into_iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                DialogOutcome::SaveProject(p) => vec![p.to_string_lossy().into_owned()],
            };
            let _ = tx.send(paths);
            return;
        }
        guard.outcomes.push(outcome);
    }
}

/// Deliver a folder path chosen via Android SAF `ACTION_OPEN_DOCUMENT_TREE`.
pub fn deliver_saf_folder_path(path: PathBuf) {
    if let Ok(mut guard) = manager().lock() {
        let reply = guard
            .saf_reply
            .take()
            .or_else(|| guard.active.take().and_then(|mut d| d.reply.take()));
        if let Some(tx) = reply {
            let _ = tx.send(vec![path.to_string_lossy().into_owned()]);
        }
    }
}

/// Deliver a save path chosen via Android SAF `ACTION_CREATE_DOCUMENT`.
pub fn deliver_saf_save_path(path: PathBuf) {
    push_outcome(DialogOutcome::SaveProject(path));
}

/// Drain all pending unsolicited dialog outcomes (called once per frame by [`crate::shell::AndroidShell`]).
#[must_use]
pub fn drain_outcomes() -> Vec<DialogOutcome> {
    match manager().lock() {
        Ok(mut guard) => std::mem::take(&mut guard.outcomes),
        Err(poisoned) => std::mem::take(&mut poisoned.into_inner().outcomes),
    }
}

/// Returns `true` if the touch file dialog modal is currently open.
#[must_use]
pub fn is_dialog_open() -> bool {
    match manager().lock() {
        Ok(guard) => guard.active.is_some(),
        Err(poisoned) => poisoned.into_inner().active.is_some(),
    }
}

/// Render the touch-friendly file/folder dialog modal if one is currently active.
pub fn show_file_dialog(ctx: &egui::Context) {
    let mut guard = match manager().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let Some(mut dialog) = guard.active.take() else {
        return;
    };
    let has_bridge = guard.bridge.is_some();
    drop(guard);

    let mut close_dialog = false;
    let mut resolved_paths: Option<Vec<PathBuf>> = None;
    let mut launch_system_saf = false;

    let screen_rect = ctx.content_rect();
    let modal_width = (screen_rect.width() * 0.90).clamp(320.0, 820.0);
    let modal_height = (screen_rect.height() * 0.84).clamp(280.0, 620.0);

    egui::Window::new(&dialog.title)
        .id(egui::Id::new("filmcraft_android_file_dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([modal_width, modal_height])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // Top bar: Permission status + Quick Bookmarks
            ui.horizontal_wrapped(|ui| {
                let perm = permissions::storage_permission_state();
                let perm_color = match perm {
                    PermissionState::Granted => egui::Color32::from_rgb(80, 200, 120),
                    PermissionState::Requested => egui::Color32::from_rgb(230, 190, 80),
                    PermissionState::Denied | PermissionState::Unknown => egui::Color32::from_rgb(220, 130, 90),
                };
                ui.colored_label(perm_color, format!("● {}", perm.label()));
                if !perm.is_granted() && ui.button("Request Permission").clicked() {
                    permissions::ensure_storage_permission();
                    dialog.refresh_entries();
                }

                if has_bridge && ui.button("📂 Android System File Picker (SAF)").clicked() {
                    launch_system_saf = true;
                }
            });

            ui.separator();

            // Storage bookmarks bar
            ui.horizontal_wrapped(|ui| {
                ui.label("Locations:");
                for (label, path) in permissions::current_dirs().bookmarks() {
                    let selected = dialog.current_dir == path;
                    if ui.selectable_label(selected, label).clicked() {
                        let _ = std::fs::create_dir_all(&path);
                        dialog.current_dir = path;
                        dialog.refresh_entries();
                    }
                }
            });

            // Filter selector bar (for Open / Save dialogs with multiple filter groups)
            if dialog.filters.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Format filter:");
                    let count = dialog.filters.len();
                    for idx in 0..count {
                        let label = dialog.filters[idx].0.clone();
                        if ui.selectable_label(dialog.active_filter_idx == idx, label).clicked() {
                            dialog.active_filter_idx = idx;
                            dialog.refresh_entries();
                        }
                    }
                });
            }

            // Current directory path + Up button + Refresh + New Folder
            ui.horizontal(|ui| {
                if ui.button("⬆ Up").clicked()
                    && let Some(parent) = dialog.current_dir.parent().map(Path::to_path_buf)
                {
                    dialog.current_dir = parent;
                    dialog.refresh_entries();
                }
                if ui.button("⟳ Refresh").clicked() {
                    dialog.refresh_entries();
                }
                if ui.button("+ New Folder").clicked() && dialog.new_folder_name.is_none() {
                    dialog.new_folder_name = Some("New Folder".to_string());
                }
                ui.monospace(dialog.current_dir.to_string_lossy());
            });

            if let Some(mut folder_name) = dialog.new_folder_name.take() {
                let mut keep_open = true;
                ui.horizontal(|ui| {
                    ui.label("Folder name:");
                    ui.text_edit_singleline(&mut folder_name);
                    if ui.button("Create").clicked() {
                        let trimmed = folder_name.trim();
                        if !trimmed.is_empty() {
                            let target = dialog.current_dir.join(trimmed);
                            match std::fs::create_dir_all(&target) {
                                Ok(()) => {
                                    dialog.current_dir = target;
                                    dialog.refresh_entries();
                                    keep_open = false;
                                }
                                Err(err) => {
                                    dialog.error_banner = Some(format!("Could not create folder: {err}"));
                                }
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
                if keep_open {
                    dialog.new_folder_name = Some(folder_name);
                }
            }

            if let Some(err) = &dialog.error_banner {
                ui.colored_label(egui::Color32::from_rgb(240, 110, 110), err);
            }

            ui.separator();

            // Directory & file listing
            let list_height = (ui.available_height() - 88.0).max(120.0);
            egui::ScrollArea::vertical()
                .id_salt("filmcraft_android_file_list")
                .max_height(list_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if dialog.entries.is_empty() {
                        ui.weak("No matching files in this folder.");
                    }
                    let entries = dialog.entries.clone();
                    for entry in entries {
                        let is_selected = dialog.selected_paths.contains(&entry.path);
                        let icon = if entry.is_dir { "📁" } else { media_icon_for_path(&entry.path) };
                        let size_label = if entry.is_dir {
                            String::new()
                        } else {
                            format!("  ({})", format_size(entry.size_bytes))
                        };
                        let row_text = format!("{icon} {}{size_label}", entry.name);
                        let resp = ui.selectable_label(is_selected, row_text);
                        if resp.clicked() {
                            if entry.is_dir {
                                dialog.current_dir.clone_from(&entry.path);
                                dialog.selected_paths.clear();
                                dialog.refresh_entries();
                            } else {
                                dialog.file_name_input.clone_from(&entry.name);
                                match dialog.kind {
                                    DialogKind::Open { multi: true } => {
                                        if let Some(pos) = dialog.selected_paths.iter().position(|p| *p == entry.path) {
                                            dialog.selected_paths.remove(pos);
                                        } else {
                                            dialog.selected_paths.push(entry.path.clone());
                                        }
                                    }
                                    _ => {
                                        dialog.selected_paths = vec![entry.path.clone()];
                                    }
                                }
                            }
                        }
                        if resp.double_clicked() && !entry.is_dir && matches!(dialog.kind, DialogKind::Open { .. }) {
                            resolved_paths = Some(vec![entry.path]);
                            close_dialog = true;
                        }
                    }
                });

            ui.separator();

            // Bottom controls: filename input or selected count + action buttons
            match dialog.kind {
                DialogKind::Save => {
                    ui.horizontal(|ui| {
                        ui.label("File name:");
                        let edit = egui::TextEdit::singleline(&mut dialog.file_name_input).desired_width(240.0);
                        ui.add(edit);

                        // Quick extension chips from the active filter
                        let exts: Vec<String> = dialog
                            .allowed_extensions()
                            .iter()
                            .take(6)
                            .cloned()
                            .collect();
                        for ext in exts {
                            let chip = format!(".{ext}");
                            let active = Path::new(&dialog.file_name_input)
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| e.eq_ignore_ascii_case(&ext));
                            if ui.selectable_label(active, chip).clicked() {
                                dialog.file_name_input = replace_extension(&dialog.file_name_input, &ext);
                            }
                        }
                    });
                }
                DialogKind::Open { multi } => {
                    ui.horizontal(|ui| {
                        if multi && !dialog.selected_paths.is_empty() {
                            ui.label(format!("Selected {} file(s)", dialog.selected_paths.len()));
                            if ui.button("Select All Files in Folder").clicked() {
                                dialog.selected_paths = dialog
                                    .entries
                                    .iter()
                                    .filter(|e| !e.is_dir)
                                    .map(|e| e.path.clone())
                                    .collect();
                            }
                            if ui.button("Clear Selection").clicked() {
                                dialog.selected_paths.clear();
                            }
                        } else {
                            ui.label("File:");
                            ui.text_edit_singleline(&mut dialog.file_name_input);
                        }
                    });
                }
                DialogKind::PickFolder => {
                    ui.horizontal(|ui| {
                        ui.label("Selected Folder:");
                        ui.monospace(dialog.current_dir.to_string_lossy());
                    });
                }
            }

            ui.horizontal(|ui| {
                let confirm_label = match dialog.kind {
                    DialogKind::Open { multi: true } if dialog.selected_paths.len() > 1 => {
                        format!("Import {} Files", dialog.selected_paths.len())
                    }
                    DialogKind::Open { .. } => "Open".to_string(),
                    DialogKind::Save => "Save / Export".to_string(),
                    DialogKind::PickFolder => "Use This Folder".to_string(),
                };

                if ui.button(confirm_label).clicked() {
                    match dialog.kind {
                        DialogKind::Open { .. } => {
                            if !dialog.selected_paths.is_empty() {
                                resolved_paths = Some(dialog.selected_paths.clone());
                                close_dialog = true;
                            } else {
                                let candidate = dialog.current_dir.join(dialog.file_name_input.trim());
                                if candidate.is_file() {
                                    resolved_paths = Some(vec![candidate]);
                                    close_dialog = true;
                                } else {
                                    dialog.error_banner = Some("Select one or more files to open.".to_string());
                                }
                            }
                        }
                        DialogKind::Save => {
                            let default_ext = dialog
                                .allowed_extensions()
                                .first()
                                .map_or("mp4", String::as_str);
                            let name = ensure_default_extension(dialog.file_name_input.trim(), default_ext);
                            let requested = dialog.current_dir.join(name);
                            let resolved = permissions::resolve_export_target(&requested);
                            resolved_paths = Some(vec![resolved]);
                            close_dialog = true;
                        }
                        DialogKind::PickFolder => {
                            let _ = std::fs::create_dir_all(&dialog.current_dir);
                            resolved_paths = Some(vec![dialog.current_dir.clone()]);
                            close_dialog = true;
                        }
                    }
                }

                if ui.button("Cancel").clicked() {
                    close_dialog = true;
                }
            });
        });

    if launch_system_saf {
        let mut guard = match manager().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let launched = if let Some(bridge) = &guard.bridge {
            match dialog.kind {
                DialogKind::Open { multi } => (bridge.launch_saf_open)(dialog.allowed_extensions(), multi),
                DialogKind::Save => {
                    let default_ext = dialog.allowed_extensions().first().map_or("mp4", String::as_str);
                    let suggested = ensure_default_extension(&dialog.file_name_input, default_ext);
                    let mime = mime_for_filename(&suggested);
                    (bridge.launch_saf_save)(&suggested, mime)
                }
                DialogKind::PickFolder => (bridge.launch_saf_folder)(),
            }
        } else {
            false
        };
        if launched {
            guard.saf_reply = dialog.reply.take();
            close_dialog = true;
        }
    }

    if let Some(paths) = resolved_paths {
        let strings: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        if let Some(tx) = dialog.reply.take() {
            let _ = tx.send(strings);
        } else {
            match dialog.kind {
                DialogKind::Open { .. } => push_outcome(DialogOutcome::OpenFiles(paths)),
                DialogKind::Save => {
                    if let Some(first) = paths.into_iter().next() {
                        push_outcome(DialogOutcome::SaveProject(first));
                    }
                }
                DialogKind::PickFolder => {}
            }
        }
    } else if close_dialog {
        if let Some(tx) = dialog.reply.take() {
            let _ = tx.send(Vec::new());
        }
    } else if let Ok(mut guard) = manager().lock() {
        guard.active = Some(dialog);
    }
}

fn media_icon_for_path(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "📄";
    };
    if ext.eq_ignore_ascii_case("fcproj") {
        "🎞"
    } else if VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext)) {
        "🎬"
    } else if AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(ext)) {
        "🎵"
    } else if IMAGE_EXTENSIONS.iter().any(|i| i.eq_ignore_ascii_case(ext)) {
        "🖼"
    } else {
        "📄"
    }
}

/// Replace or append an extension on a filename.
#[must_use]
pub fn replace_extension(name: &str, ext: &str) -> String {
    let stem = Path::new(name.trim())
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Export");
    format!("{stem}.{ext}")
}

/// Ensure a filename has an extension, appending `default_ext` if none is present.
#[must_use]
pub fn ensure_default_extension(name: &str, default_ext: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return format!("Export.{default_ext}");
    }
    if Path::new(trimmed).extension().is_some() {
        trimmed.to_string()
    } else {
        format!("{trimmed}.{default_ext}")
    }
}

/// Infer the Android MIME type for a filename (used for SAF `ACTION_CREATE_DOCUMENT` and
/// `MediaScannerConnection`).
#[must_use]
pub fn mime_for_filename(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mxf") => "application/mxf",
        Some("ts" | "m2ts" | "mts") => "video/mp2t",
        Some("mpg" | "mpeg") => "video/mpeg",
        Some("wav") => "audio/wav",
        Some("aif" | "aiff") => "audio/x-aiff",
        Some("flac") => "audio/flac",
        Some("mp3") => "audio/mpeg",
        Some("aac") => "audio/aac",
        Some("m4a") => "audio/mp4",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("ac3") => "audio/ac3",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        Some("dng") => "image/x-adobe-dng",
        Some("srt") => "application/x-subrip",
        Some("vtt") => "text/vtt",
        Some("xml" | "fcpxml") => "application/xml",
        Some("csv") => "text/csv",
        Some("json" | "otio" | "fcproj") => "application/json",
        _ => "application/octet-stream",
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_helpers_preserve_or_replace_suffixes() {
        assert_eq!(ensure_default_extension("RoughCut", "fcproj"), "RoughCut.fcproj");
        assert_eq!(ensure_default_extension("Master.mov", "mp4"), "Master.mov");
        assert_eq!(replace_extension("Master.mov", "mp4"), "Master.mp4");
        assert_eq!(mime_for_filename("Sequence 01.mp4"), "video/mp4");
        assert_eq!(mime_for_filename("Trailer.mov"), "video/quicktime");
        assert_eq!(mime_for_filename("Dialogue.wav"), "audio/wav");
        assert_eq!(mime_for_filename("Project.fcproj"), "application/json");
    }

    #[test]
    fn matches_extension_filter_is_case_insensitive_and_ignores_linux_globs() {
        let allowed = vec!["mp4".to_string(), "mov".to_string(), "[mM][pP]4".to_string()];
        assert!(matches_extension_filter(Path::new("/storage/Movies/CLIP001.MP4"), &allowed));
        assert!(matches_extension_filter(Path::new("/storage/Movies/take.Mov"), &allowed));
        assert!(!matches_extension_filter(Path::new("/storage/Movies/notes.txt"), &allowed));
    }

    #[test]
    fn open_native_dialog_connects_reply_channel_to_saf_delivery() {
        let (tx, rx) = std::sync::mpsc::channel();
        open_native_dialog(
            FileDialog::SaveAs {
                filter: "MP4".into(),
                exts: vec!["mp4".into()],
                name: "Sequence 01.mp4".into(),
            },
            tx,
        );
        assert!(is_dialog_open());
        deliver_saf_save_path(PathBuf::from("/storage/emulated/0/Movies/Sequence 01.mp4"));
        assert_eq!(rx.try_recv().unwrap(), vec!["/storage/emulated/0/Movies/Sequence 01.mp4".to_string()]);
        assert!(!is_dialog_open());
    }
}
