//! Android `eframe::App` shell wrapping [`filmcraft_ui_egui::FilmcraftApp`] with:
//! - Hardware GPU compositing (`app.set_wgpu(rs)` + `filmcraft_export::register_frame_renderer`)
//! - Startup GPU crash-recovery sentinel clearing on the first completed frame
//! - Touch-twin event deduplication (`drop_touch_twins`) when Android stylus axes are enabled
//! - Touch file browser modal + Android SAF outcome handling (`file_dialog::show_file_dialog`)
//! - Writable Android export directory normalization and `MediaScannerConnection` notifications for
//!   finished video/audio exports

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use filmcraft_engine::Session;
use filmcraft_engine::autosave::AutosaveConfig;
use filmcraft_ui_egui::FilmcraftApp;
use serde_json::json;

use crate::audio::CpalOut;
use crate::audio_in::CpalIn;
use crate::file_dialog::{self, DialogOutcome};
use crate::gpu_startup::GpuStartupPlan;
use crate::permissions;
use crate::services::{AndroidServices, android_hooks};

/// GPU off-screen frame renderer for video exports (`filmcraft_gpu::ExportRenderer`).
struct GpuFrameRenderer(filmcraft_gpu::ExportRenderer);

impl filmcraft_export::FrameRenderer for GpuFrameRenderer {
    fn render(
        &mut self,
        project: &filmcraft_project::Project,
        seq: filmcraft_project::ItemId,
        t: filmcraft_time::Tick,
        opts: filmcraft_render::RenderOptions,
        sources: &dyn filmcraft_render::SourceProvider,
    ) -> Option<filmcraft_render::Image> {
        self.0.render(project, seq, t, opts, sources)
    }
}

/// Top-level Android `eframe::App` wrapper around [`FilmcraftApp`].
pub struct AndroidShell {
    pub app: FilmcraftApp,
    gpu_plan: GpuStartupPlan,
    first_frame_completed: bool,
    scanned_export_jobs: HashSet<u64>,
}

impl AndroidShell {
    /// Construct the Android shell and initialize [`FilmcraftApp`], audio I/O, GPU compositors,
    /// persistence, and initial project state.
    #[must_use]
    pub fn create(
        cc: &eframe::CreationContext<'_>,
        gpu_plan: GpuStartupPlan,
        initial_file: Option<PathBuf>,
    ) -> Self {
        let _ = filmcraft_platform::register();
        filmcraft_ui_egui::install_ime_reset_callbacks(&cc.egui_ctx);

        let dirs = permissions::current_dirs();
        let data_dir = dirs.internal_data.clone();

        let mut session = Session::new(Arc::new(AndroidServices));
        let _ = session.start_autosave(AutosaveConfig::new(data_dir));
        let hw = session.prefs.audio_hardware.clone();
        let seq_rate = session.active_sequence().map(|q| q.settings.sample_rate);
        session.voiceover.input = Some(Box::new(CpalIn::new(&hw.device_class)));

        let mut opened_initial = false;
        if let Some(path) = initial_file.as_deref()
            && path.exists()
        {
            let path_str = path.to_string_lossy().into_owned();
            if is_project_file(path) {
                if session.execute("file.open", json!({ "path": path_str })).is_ok() {
                    opened_initial = true;
                }
            } else {
                let _ = session.execute("file.openDemoProject", json!({}));
                let _ = session.execute("file.import", json!({ "paths": [path_str] }));
                opened_initial = true;
            }
        }

        if !opened_initial {
            let _ = session.execute("file.openDemoProject", json!({}));
        }

        let mut app = FilmcraftApp::new(session);
        let mut cpal_out = CpalOut::new();
        use filmcraft_ui_egui::AudioOut;
        cpal_out.configure(&hw, seq_rate);
        app.audio = Some(Box::new(cpal_out));
        app.hooks = android_hooks();
        app.ui.export.location = dirs.default_save_dir().to_string_lossy().into_owned();

        // Enable hardware GPU compositing for both real-time monitors and video exports.
        if gpu_plan.backend.is_hardware_accelerated()
            && let Some(rs) = cc.wgpu_render_state.clone()
        {
            app.set_wgpu(rs);
            filmcraft_export::register_frame_renderer(|| {
                filmcraft_gpu::ExportRenderer::new()
                    .map(|r| Box::new(GpuFrameRenderer(r)) as Box<dyn filmcraft_export::FrameRenderer>)
            });
        }

        app.ui.status = gpu_plan.status_note();

        Self {
            app,
            gpu_plan,
            first_frame_completed: false,
            scanned_export_jobs: HashSet::new(),
        }
    }

    /// Process any unsolicited file-open or project-save outcomes from external Android intents.
    fn drain_dialog_outcomes(&mut self) {
        for outcome in file_dialog::drain_outcomes() {
            match outcome {
                DialogOutcome::OpenFiles(paths) => {
                    let mut media_paths = Vec::new();
                    for path in paths {
                        let path_str = path.to_string_lossy().into_owned();
                        if is_project_file(&path) {
                            match self.app.session.execute("file.open", json!({ "path": path_str })) {
                                Ok(_) => {
                                    self.app.ui.status = format!("Opened project {}", path.display());
                                }
                                Err(err) => {
                                    self.app.ui.status = format!("Open project failed: {err}");
                                }
                            }
                        } else {
                            media_paths.push(path_str);
                        }
                    }
                    if !media_paths.is_empty() {
                        let count = media_paths.len();
                        match self.app.session.execute("file.import", json!({ "paths": media_paths })) {
                            Ok(_) => {
                                self.app.ui.status = format!("Imported {count} file(s)");
                            }
                            Err(err) => {
                                self.app.ui.status = format!("Import failed: {err}");
                            }
                        }
                    }
                }
                DialogOutcome::SaveProject(path) => {
                    let resolved = permissions::resolve_export_target(&path);
                    let path_str = resolved.to_string_lossy().into_owned();
                    match self.app.session.execute("file.save", json!({ "path": path_str })) {
                        Ok(_) => {
                            file_dialog::notify_media_scanner(&resolved);
                            self.app.ui.status = format!("Saved {}", resolved.display());
                        }
                        Err(err) => {
                            self.app.ui.status = format!("Save failed: {err}");
                        }
                    }
                }
            }
        }
    }

    /// Ensure `app.ui.export.location` always points to a writable Android directory so the Export
    /// mode "Export" and "Send to Queue" buttons work out-of-the-box under scoped storage.
    fn normalize_export_location(&mut self) {
        let loc = self.app.ui.export.location.trim();
        if loc.is_empty() || !permissions::is_dir_writable(Path::new(loc)) {
            self.app.ui.export.location = permissions::current_dirs()
                .default_save_dir()
                .to_string_lossy()
                .into_owned();
        }
    }

    /// Notify Android's `MediaScannerConnection` whenever a background export job finishes writing
    /// an output file so the exported video/audio appears in the system Gallery / Movies / Files
    /// app immediately.
    fn notify_finished_exports(&mut self) {
        for job in &self.app.session.jobs {
            if !job.progress.finished.load(Ordering::Relaxed) || self.scanned_export_jobs.contains(&job.id) {
                continue;
            }
            self.scanned_export_jobs.insert(job.id);
            let Ok(guard) = job.result.lock() else {
                continue;
            };
            if let Some(Ok(report)) = guard.as_ref() {
                if !report.path.is_empty() {
                    file_dialog::notify_media_scanner(Path::new(&report.path));
                }
                for extra in &report.extra_files {
                    if !extra.is_empty() {
                        file_dialog::notify_media_scanner(Path::new(extra));
                    }
                }
            }
        }
    }
}

fn is_project_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("fcproj"))
}

/// Remove synthetic mouse events that `winit` emits alongside `Event::Touch` when stylus axes are
/// enabled on Android, preventing double-taps on `egui` buttons and menus.
pub fn drop_touch_twins(raw_input: &mut egui::RawInput) {
    let has_touch = raw_input
        .events
        .iter()
        .any(|e| matches!(e, egui::Event::Touch { .. }));
    if !has_touch {
        return;
    }

    let touch_positions: Vec<egui::Pos2> = raw_input
        .events
        .iter()
        .filter_map(|e| match e {
            egui::Event::Touch { pos, .. } => Some(*pos),
            _ => None,
        })
        .collect();

    let mut seen_primary_press = false;
    let mut seen_primary_release = false;

    raw_input.events.retain(|event| match event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            ..
        } if touch_positions.iter().any(|tp| (*tp - *pos).length_sq() <= 4.0) => {
            if *pressed {
                if seen_primary_press {
                    return false;
                }
                seen_primary_press = true;
            } else {
                if seen_primary_release {
                    return false;
                }
                seen_primary_release = true;
            }
            true
        }
        _ => true,
    });
}

impl eframe::App for AndroidShell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        drop_touch_twins(raw_input);
        self.app.raw_input_hook(ctx, raw_input);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_dialog_outcomes();
        self.normalize_export_location();
        self.app.logic(ctx, frame);
        self.notify_finished_exports();
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        file_dialog::show_file_dialog(ui.ctx());

        if !self.first_frame_completed {
            self.first_frame_completed = true;
            self.gpu_plan.mark_startup_succeeded();
        }
    }

    fn on_exit(&mut self) {
        self.app.on_exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_touch_twins_deduplicates_duplicate_primary_pointer_events() {
        let pos = egui::pos2(120.0, 240.0);
        let mut raw = egui::RawInput {
            events: vec![
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(1),
                    phase: egui::TouchPhase::Start,
                    pos,
                    force: Some(0.5),
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        drop_touch_twins(&mut raw);
        let presses = raw
            .events
            .iter()
            .filter(|e| matches!(e, egui::Event::PointerButton { pressed: true, .. }))
            .count();
        assert_eq!(presses, 1);
    }
}
