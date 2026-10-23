//! Android runtime file/media/audio permission state, shared storage directory discovery, and
//! scoped-storage fallback resolution for importing and exporting video, audio, still images,
//! captions, LUTs, and `.fcproj` projects.
//!
//! Android's storage and media permission model spans three API generations:
//! - **API 26–28 (Android 8.0–9)**: `READ_EXTERNAL_STORAGE` and `WRITE_EXTERNAL_STORAGE` grant
//!   direct POSIX access to shared storage (`/storage/emulated/0/{Movies,DCIM,Download,Pictures,Music,Documents}`).
//! - **API 29 (Android 10)**: `requestLegacyExternalStorage="true"` preserves POSIX access while
//!   scoped storage is transitional.
//! - **API 30–32 (Android 11–12L)**: Scoped storage enforced; `READ_EXTERNAL_STORAGE` grants media
//!   reads, and app-specific directories + public `Movies` / `Download` / SAF work for exports.
//! - **API 33+ (Android 13–14+)**: Granular media permissions (`READ_MEDIA_VIDEO`,
//!   `READ_MEDIA_AUDIO`, `READ_MEDIA_IMAGES`) plus `MANAGE_EXTERNAL_STORAGE` for full NLE media
//!   access and `RECORD_AUDIO` for timeline voice-over recording.
//!
//! Regardless of whether the user grants runtime storage permissions, FilmCraft can always:
//! 1. Read and write inside its app-specific internal (`Context.getFilesDir()`) and external
//!    (`Context.getExternalFilesDir(null)`) directories without any runtime permission.
//! 2. Import and export any user-chosen file or folder anywhere on the device (including SD cards
//!    and cloud providers) via the Android Storage Access Framework (SAF) system picker.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Android permission constants declared in `AndroidManifest.xml`.
pub mod manifest_permissions {
    pub const READ_EXTERNAL_STORAGE: &str = "android.permission.READ_EXTERNAL_STORAGE";
    pub const WRITE_EXTERNAL_STORAGE: &str = "android.permission.WRITE_EXTERNAL_STORAGE";
    pub const READ_MEDIA_VIDEO: &str = "android.permission.READ_MEDIA_VIDEO";
    pub const READ_MEDIA_AUDIO: &str = "android.permission.READ_MEDIA_AUDIO";
    pub const READ_MEDIA_IMAGES: &str = "android.permission.READ_MEDIA_IMAGES";
    pub const MANAGE_EXTERNAL_STORAGE: &str = "android.permission.MANAGE_EXTERNAL_STORAGE";
    pub const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";

    /// All storage/media permissions relevant to importing and exporting video, audio, still
    /// images, LUTs, captions, and project files.
    pub const ALL_STORAGE_PERMISSIONS: &[&str] = &[
        READ_EXTERNAL_STORAGE,
        WRITE_EXTERNAL_STORAGE,
        READ_MEDIA_VIDEO,
        READ_MEDIA_AUDIO,
        READ_MEDIA_IMAGES,
        MANAGE_EXTERNAL_STORAGE,
    ];
}

/// Current state of Android runtime permissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionState {
    /// Permission has not been queried or requested yet.
    #[default]
    Unknown,
    /// A system permission prompt is currently in flight.
    Requested,
    /// Permission was granted by the user or is implicitly granted on this API level.
    Granted,
    /// Permission was denied by the user; app-specific storage and SAF remain usable.
    Denied,
}

impl PermissionState {
    #[must_use]
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Not requested",
            Self::Requested => "Requesting permission…",
            Self::Granted => "Storage access granted",
            Self::Denied => "Scoped storage / SAF mode (permission denied)",
        }
    }
}

/// Standard Android directories discovered at startup.
#[derive(Clone, Debug, Default)]
pub struct AndroidDirs {
    /// App-private internal storage (`/data/user/0/ai.storyteller.filmcraft/files`).
    pub internal_data: PathBuf,
    /// App-specific external storage (`/storage/emulated/0/Android/data/ai.storyteller.filmcraft/files`).
    pub external_data: Option<PathBuf>,
    /// Shared external storage root (`/storage/emulated/0`).
    pub shared_root: Option<PathBuf>,
    /// Shared `Movies` directory (`/storage/emulated/0/Movies`).
    pub movies: Option<PathBuf>,
    /// Shared `Download` directory (`/storage/emulated/0/Download`).
    pub downloads: Option<PathBuf>,
    /// Shared `DCIM` camera directory (`/storage/emulated/0/DCIM`).
    pub dcim: Option<PathBuf>,
    /// Shared `Pictures` directory (`/storage/emulated/0/Pictures`).
    pub pictures: Option<PathBuf>,
    /// Shared `Music` directory (`/storage/emulated/0/Music`).
    pub music: Option<PathBuf>,
    /// Shared `Documents` directory (`/storage/emulated/0/Documents`).
    pub documents: Option<PathBuf>,
}

impl AndroidDirs {
    /// Discover Android directories from internal/external paths and standard environment roots.
    #[must_use]
    pub fn discover(internal_data: PathBuf, external_data: Option<PathBuf>) -> Self {
        let shared_root = external_data
            .as_deref()
            .and_then(infer_shared_root_from_app_external)
            .or_else(|| {
                std::env::var_os("EXTERNAL_STORAGE")
                    .map(PathBuf::from)
                    .filter(|p| p.exists())
            })
            .or_else(|| {
                let candidate = PathBuf::from("/storage/emulated/0");
                candidate.exists().then_some(candidate)
            });

        let movies = shared_root.as_ref().map(|r| r.join("Movies"));
        let downloads = shared_root.as_ref().map(|r| r.join("Download"));
        let dcim = shared_root.as_ref().map(|r| r.join("DCIM"));
        let pictures = shared_root.as_ref().map(|r| r.join("Pictures"));
        let music = shared_root.as_ref().map(|r| r.join("Music"));
        let documents = shared_root.as_ref().map(|r| r.join("Documents"));

        Self {
            internal_data,
            external_data,
            shared_root,
            movies,
            downloads,
            dcim,
            pictures,
            music,
            documents,
        }
    }

    /// Best default directory for opening/importing video, audio, and project files.
    #[must_use]
    pub fn default_open_dir(&self) -> PathBuf {
        for candidate in [&self.movies, &self.dcim, &self.downloads, &self.documents, &self.pictures, &self.music, &self.shared_root]
            .into_iter()
            .flatten()
        {
            if candidate.exists() && is_dir_readable(candidate) {
                return candidate.clone();
            }
        }
        if let Some(ext) = &self.external_data {
            let _ = std::fs::create_dir_all(ext);
            if ext.exists() {
                return ext.clone();
            }
        }
        let _ = std::fs::create_dir_all(&self.internal_data);
        self.internal_data.clone()
    }

    /// Best default directory for exporting media (`.mp4`, `.mov`, `.wav`, etc.) and saving
    /// `.fcproj` projects.
    ///
    /// Prefers public `Movies` or `Download` when writable, falling back to app-specific external
    /// `Exports` or internal `Exports` so exports always succeed even under strict scoped storage.
    #[must_use]
    pub fn default_save_dir(&self) -> PathBuf {
        for candidate in [&self.movies, &self.downloads, &self.documents].into_iter().flatten() {
            if is_dir_writable(candidate) {
                return candidate.clone();
            }
        }
        if let Some(ext) = &self.external_data {
            let exports = ext.join("Exports");
            if std::fs::create_dir_all(&exports).is_ok() && is_dir_writable(&exports) {
                return exports;
            }
        }
        let internal_exports = self.internal_data.join("Exports");
        let _ = std::fs::create_dir_all(&internal_exports);
        internal_exports
    }

    /// Named bookmark locations for the touch file dialog and the FilmCraft Media Browser panel.
    #[must_use]
    pub fn bookmarks(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        if let Some(movies) = &self.movies {
            out.push(("Movies".to_string(), movies.clone()));
        }
        if let Some(dcim) = &self.dcim {
            out.push(("Camera (DCIM)".to_string(), dcim.clone()));
        }
        if let Some(dl) = &self.downloads {
            out.push(("Downloads".to_string(), dl.clone()));
        }
        if let Some(pics) = &self.pictures {
            out.push(("Pictures".to_string(), pics.clone()));
        }
        if let Some(music) = &self.music {
            out.push(("Music".to_string(), music.clone()));
        }
        if let Some(docs) = &self.documents {
            out.push(("Documents".to_string(), docs.clone()));
        }
        if let Some(shared) = &self.shared_root {
            out.push(("Shared Storage".to_string(), shared.clone()));
        }
        if let Some(ext) = &self.external_data {
            out.push(("App External".to_string(), ext.clone()));
            out.push(("App Exports".to_string(), ext.join("Exports")));
        }
        out.push(("App Internal".to_string(), self.internal_data.clone()));
        out
    }
}

/// Infer `/storage/emulated/0` from `/storage/emulated/0/Android/data/<pkg>/files`.
#[must_use]
pub fn infer_shared_root_from_app_external(app_external: &Path) -> Option<PathBuf> {
    let mut current = Some(app_external);
    while let Some(dir) = current {
        if dir.file_name().and_then(|s| s.to_str()) == Some("Android") {
            return dir.parent().map(Path::to_path_buf);
        }
        current = dir.parent();
    }
    None
}

/// Check whether a directory can be listed via POSIX `read_dir`.
#[must_use]
pub fn is_dir_readable(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok()
}

/// Check whether a directory allows creating and removing a temporary probe file.
#[must_use]
pub fn is_dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".filmcraft_write_probe_{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Global permission & directory state shared between the JNI bridge, `AndroidServices`, and UI.
struct GlobalPermissionStatus {
    storage: PermissionState,
    audio_record: PermissionState,
    dirs: AndroidDirs,
}

fn global_status() -> &'static Mutex<GlobalPermissionStatus> {
    static STATUS: OnceLock<Mutex<GlobalPermissionStatus>> = OnceLock::new();
    STATUS.get_or_init(|| {
        let fallback_dir = std::env::temp_dir().join("filmcraft-android");
        let _ = std::fs::create_dir_all(&fallback_dir);
        Mutex::new(GlobalPermissionStatus {
            storage: PermissionState::Unknown,
            audio_record: PermissionState::Unknown,
            dirs: AndroidDirs::discover(fallback_dir, None),
        })
    })
}

/// Initialize the Android directory layout at startup.
pub fn init_dirs(internal_data: PathBuf, external_data: Option<PathBuf>) {
    if let Ok(mut guard) = global_status().lock() {
        guard.dirs = AndroidDirs::discover(internal_data, external_data);
    }
}

/// Snapshot the current Android directories.
#[must_use]
pub fn current_dirs() -> AndroidDirs {
    match global_status().lock() {
        Ok(guard) => guard.dirs.clone(),
        Err(poisoned) => poisoned.into_inner().dirs.clone(),
    }
}

/// Query the current storage permission state.
#[must_use]
pub fn storage_permission_state() -> PermissionState {
    match global_status().lock() {
        Ok(guard) => guard.storage,
        Err(poisoned) => poisoned.into_inner().storage,
    }
}

/// Record a new storage permission state (called from JNI or permission requester).
pub fn set_storage_permission_state(state: PermissionState) {
    if let Ok(mut guard) = global_status().lock() {
        guard.storage = state;
    }
}

/// Query the current microphone (`RECORD_AUDIO`) permission state.
#[must_use]
pub fn audio_record_permission_state() -> PermissionState {
    match global_status().lock() {
        Ok(guard) => guard.audio_record,
        Err(poisoned) => poisoned.into_inner().audio_record,
    }
}

/// Record a new microphone (`RECORD_AUDIO`) permission state.
pub fn set_audio_record_permission_state(state: PermissionState) {
    if let Ok(mut guard) = global_status().lock() {
        guard.audio_record = state;
    }
}

/// Ensure storage permissions are requested before an import or export operation.
///
/// Returns `true` if storage access is ready (either already granted or shared storage is
/// readable/writable), or `false` if a permission prompt was just triggered.
pub fn ensure_storage_permission() -> bool {
    let current = storage_permission_state();
    if current.is_granted() {
        return true;
    }

    let dirs = current_dirs();
    if dirs.movies.as_deref().is_some_and(is_dir_readable)
        || dirs.downloads.as_deref().is_some_and(is_dir_readable)
        || dirs.shared_root.as_deref().is_some_and(is_dir_readable)
    {
        set_storage_permission_state(PermissionState::Granted);
        return true;
    }

    if crate::file_dialog::request_platform_storage_permissions() {
        return true;
    }

    false
}

/// Ensure `RECORD_AUDIO` permission is requested before starting a timeline voice-over recording.
pub fn ensure_audio_record_permission() -> bool {
    if audio_record_permission_state().is_granted() {
        return true;
    }
    if crate::file_dialog::request_platform_audio_permission() {
        return true;
    }
    false
}

/// Resolve a requested export/save path so that if the target parent directory is not writable
/// (for instance, due to Android 11+ scoped storage restrictions on an arbitrary path), it
/// gracefully falls back to `default_save_dir()` while preserving the file name.
#[must_use]
pub fn resolve_export_target(requested: &Path) -> PathBuf {
    let file_name = requested
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Export.mp4");

    if let Some(parent) = requested.parent()
        && !parent.as_os_str().is_empty()
        && is_dir_writable(parent)
    {
        return requested.to_path_buf();
    }

    let dirs = current_dirs();
    if requested.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        return dirs.default_save_dir().join(file_name);
    }

    ensure_storage_permission();
    if let Some(parent) = requested.parent()
        && !parent.as_os_str().is_empty()
        && is_dir_writable(parent)
    {
        return requested.to_path_buf();
    }

    dirs.default_save_dir().join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_shared_root_from_android_app_external_dir() {
        let ext = PathBuf::from("/storage/emulated/0/Android/data/ai.storyteller.filmcraft/files");
        assert_eq!(infer_shared_root_from_app_external(&ext), Some(PathBuf::from("/storage/emulated/0")));
    }

    #[test]
    fn discovers_bookmarks_and_writable_save_dir() {
        let temp = tempfile::tempdir().unwrap();
        let internal = temp.path().join("internal");
        let external = temp.path().join("storage/emulated/0/Android/data/ai.storyteller.filmcraft/files");
        std::fs::create_dir_all(&internal).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let dirs = AndroidDirs::discover(internal.clone(), Some(external));
        assert_eq!(dirs.shared_root, Some(temp.path().join("storage/emulated/0")));
        let save_dir = dirs.default_save_dir();
        assert!(is_dir_writable(&save_dir));
        let bookmarks = dirs.bookmarks();
        assert!(bookmarks.iter().any(|(label, _)| label == "Movies"));
        assert!(bookmarks.iter().any(|(label, _)| label == "Camera (DCIM)"));
        assert!(bookmarks.iter().any(|(label, _)| label == "Downloads"));
        assert!(bookmarks.iter().any(|(label, _)| label == "App Internal"));
    }

    #[test]
    fn resolve_export_target_preserves_writable_path_and_falls_back_for_relative_name() {
        let temp = tempfile::tempdir().unwrap();
        init_dirs(temp.path().join("internal"), Some(temp.path().join("external")));

        let writable_dir = temp.path().join("custom_exports");
        std::fs::create_dir_all(&writable_dir).unwrap();
        let explicit = writable_dir.join("master.mp4");
        assert_eq!(resolve_export_target(&explicit), explicit);

        let relative = PathBuf::from("timeline_edit.fcproj");
        let resolved = resolve_export_target(&relative);
        assert_eq!(resolved.file_name().and_then(|s| s.to_str()), Some("timeline_edit.fcproj"));
        assert!(resolved.parent().is_some_and(is_dir_writable));
    }
}
