//! Android implementation of [`filmcraft_engine::Services`] and [`filmcraft_ui_egui::HostHooks`]
//! with runtime storage permission checks, scoped-storage fallback, atomic writes, Media Browser
//! volume discovery, and `MediaScannerConnection` notifications for exported media.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::io::Write;
use std::path::{Path, PathBuf};

use filmcraft_engine::Services;
use filmcraft_engine::media_browser::{Volume, VolumeKind};
use filmcraft_ui_egui::HostHooks;
use filmcraft_ui_egui::native_dialogs::FileDialog;

use crate::file_dialog::{self, DialogOutcome};
use crate::permissions;

/// Android filesystem services for the FilmCraft engine.
pub struct AndroidServices;

impl Services for AndroidServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let target = Path::new(path);
        match std::fs::read(target) {
            Ok(bytes) => Ok(bytes),
            Err(_first_err) => {
                permissions::ensure_storage_permission();
                std::fs::read(target)
            }
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        let requested = Path::new(path);
        let resolved = permissions::resolve_export_target(requested);
        if let Some(parent) = resolved.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        filmcraft_format::atomic_write(&resolved, data)?;
        file_dialog::notify_media_scanner(&resolved);
        Ok(())
    }

    fn write_file_new(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        let requested = Path::new(path);
        let resolved = permissions::resolve_export_target(requested);
        if let Some(parent) = resolved.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&resolved)?;
        f.write_all(data).and_then(|()| f.sync_all())?;
        file_dialog::notify_media_scanner(&resolved);
        Ok(())
    }

    fn volumes(&self) -> Vec<Volume> {
        permissions::current_dirs()
            .bookmarks()
            .into_iter()
            .map(|(name, path)| Volume {
                name,
                path: path.to_string_lossy().into_owned(),
                kind: VolumeKind::Local,
            })
            .collect()
    }

    fn home_dir(&self) -> Option<String> {
        Some(
            permissions::current_dirs()
                .default_open_dir()
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Build [`HostHooks`] wired to the non-blocking Android touch file browser modal and SAF system
/// picker.
#[must_use]
pub fn android_hooks() -> HostHooks {
    HostHooks {
        run_dialog: Some(Box::new(|dialog| {
            let (tx, rx) = std::sync::mpsc::channel();
            file_dialog::open_native_dialog(dialog, tx);
            rx
        })),
        pick_files: Some(Box::new(|exts: &[&str]| {
            let (tx, _rx) = std::sync::mpsc::channel();
            let exts = exts.iter().map(|s| (*s).to_string()).collect();
            file_dialog::open_native_dialog(FileDialog::OpenFiles { exts }, tx);
            Vec::new()
        })),
        pick_open_file: Some(Box::new(|filter: &str, exts: &[&str]| {
            let (tx, _rx) = std::sync::mpsc::channel();
            let exts = exts.iter().map(|s| (*s).to_string()).collect();
            file_dialog::open_native_dialog(
                FileDialog::OpenFile {
                    filter: filter.to_string(),
                    exts,
                },
                tx,
            );
            None
        })),
        pick_open_project: Some(Box::new(|| {
            let (tx, _rx) = std::sync::mpsc::channel();
            file_dialog::open_native_dialog(FileDialog::OpenProject, tx);
            None
        })),
        pick_save: Some(Box::new(|default_name: &str| {
            let (tx, _rx) = std::sync::mpsc::channel();
            file_dialog::open_native_dialog(
                FileDialog::SaveProject {
                    name: default_name.to_string(),
                },
                tx,
            );
            None
        })),
        pick_save_as: Some(Box::new(|filter: &str, exts: &[&str], name: &str| {
            let (tx, _rx) = std::sync::mpsc::channel();
            let exts = exts.iter().map(|s| (*s).to_string()).collect();
            file_dialog::open_native_dialog(
                FileDialog::SaveAs {
                    filter: filter.to_string(),
                    exts,
                    name: name.to_string(),
                },
                tx,
            );
            None
        })),
        pick_folder: Some(Box::new(|| {
            let (tx, _rx) = std::sync::mpsc::channel();
            file_dialog::open_native_dialog(FileDialog::Folder { at: None }, tx);
            None
        })),
        pick_folder_at: Some(Box::new(|dir: &str| {
            let (tx, _rx) = std::sync::mpsc::channel();
            file_dialog::open_native_dialog(
                FileDialog::Folder {
                    at: Some(dir.to_string()),
                },
                tx,
            );
            None
        })),
        pick_file_for_relink: Some(Box::new(|exts: &[&str], _hint| {
            let (tx, _rx) = std::sync::mpsc::channel();
            let exts = exts.iter().map(|s| (*s).to_string()).collect();
            file_dialog::open_native_dialog(
                FileDialog::OpenFile {
                    filter: "Media".to_string(),
                    exts,
                },
                tx,
            );
            None
        })),
        system_languages: Some(Box::new(|| sys_locale::get_locales().collect())),
        ..HostHooks::default()
    }
}

/// Stage an imported file received from an Android `content://` URI into the app's local `imports/`
/// cache directory and return its POSIX path so FilmCraft can seek, decode, and edit it.
pub fn stage_imported_bytes(display_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let safe_name: String = display_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file_name = if safe_name.trim().is_empty() {
        "imported_media.mp4"
    } else {
        safe_name.trim()
    };

    let imports_dir = permissions::current_dirs().internal_data.join("imports");
    std::fs::create_dir_all(&imports_dir).map_err(|e| e.to_string())?;
    let target = imports_dir.join(file_name);
    filmcraft_format::atomic_write(&target, bytes).map_err(|e| e.to_string())?;
    Ok(target)
}

/// Stage bytes delivered by Android SAF `ACTION_OPEN_DOCUMENT` or `ACTION_VIEW` and dispatch them
/// to the waiting dialog channel or unsolicited outcome queue.
pub fn deliver_saf_open_bytes(display_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = stage_imported_bytes(display_name, bytes)?;
    file_dialog::push_outcome(DialogOutcome::OpenFiles(vec![path.clone()]));
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_services_round_trips_files_and_stages_saf_imports() {
        let temp = tempfile::tempdir().unwrap();
        permissions::init_dirs(temp.path().join("internal"), Some(temp.path().join("external")));

        let svc = AndroidServices;
        svc.write_file("test_project.fcproj", b"{\"version\":1}").unwrap();
        let expected_path = permissions::resolve_export_target(Path::new("test_project.fcproj"));
        let read_back = svc.read_file(&expected_path.to_string_lossy()).unwrap();
        assert_eq!(read_back, b"{\"version\":1}");

        let staged = stage_imported_bytes("clip_01.mp4", b"ISOBMFF").unwrap();
        assert!(staged.exists());
        assert_eq!(svc.read_file(&staged.to_string_lossy()).unwrap(), b"ISOBMFF");

        let volumes = svc.volumes();
        assert!(!volumes.is_empty());
        assert!(svc.home_dir().is_some());
    }
}
