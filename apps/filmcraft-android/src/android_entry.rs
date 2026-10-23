//! Android NativeActivity entrypoint (`android_main`) and JNI callbacks from `MainActivity.kt`.
//!
//! Responsibilities:
//! 1. Enable Android stylus axes (`Pressure`, `Tilt`, `Orientation`, `Distance`) on `AndroidApp`.
//! 2. Discover internal and external storage directories (`AndroidDirs`) and initialize
//!    `permissions`.
//! 3. Install the JNI [`PlatformBridge`](crate::file_dialog::PlatformBridge) so Rust can invoke
//!    `MainActivity` methods for runtime storage/media/audio permissions, Android SAF file/folder
//!    pickers, and `MediaScannerConnection` indexing.
//! 4. Configure `eframe::NativeOptions` with hardware GPU acceleration ON (`Vulkan` + `GLES`,
//!    `HighPerformance`, full hardware adapter texture/compute limits unlocked) via
//!    [`crate::gpu_startup::configure`].
//! 5. Export JNI functions called by `MainActivity.kt` when permission results, SAF URIs, or
//!    incoming `ACTION_VIEW` / `ACTION_SEND` intents arrive.

#![allow(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

#[cfg(target_os = "android")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "android")]
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "android")]
use android_activity::AndroidApp;
#[cfg(target_os = "android")]
use android_activity::input::Axis;

#[cfg(target_os = "android")]
use crate::file_dialog::{self, DialogOutcome, PlatformBridge};
#[cfg(target_os = "android")]
use crate::gpu_startup;
#[cfg(target_os = "android")]
use crate::permissions::{self, PermissionState};
#[cfg(target_os = "android")]
use crate::services;
#[cfg(target_os = "android")]
use crate::shell::AndroidShell;

#[cfg(target_os = "android")]
fn global_android_app() -> &'static Mutex<Option<AndroidApp>> {
    static APP: OnceLock<Mutex<Option<AndroidApp>>> = OnceLock::new();
    APP.get_or_init(|| Mutex::new(None))
}

/// Enable stylus axes (`Pressure`, `Tilt`, `Orientation`, `Distance`) on the `AndroidApp` motion
/// filter so pen and touch gestures carry pressure and tilt metadata.
#[cfg(target_os = "android")]
fn enable_stylus_axes(app: &AndroidApp) {
    for axis in [Axis::Pressure, Axis::Tilt, Axis::Orientation, Axis::Distance] {
        app.enable_motion_axis(axis);
    }
}

/// NativeActivity entrypoint invoked by `android-activity`.
#[cfg(target_os = "android")]
// SAFETY: `android_main` is the canonical entrypoint symbol expected by `android-activity` for
// `NativeActivity`. No other crate in the workspace defines `android_main`.
#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    enable_stylus_axes(&app);

    let internal_data = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/data/ai.storyteller.filmcraft/files"));
    let external_data = app.external_data_path();
    let _ = std::fs::create_dir_all(&internal_data);
    if let Some(ext) = &external_data {
        let _ = std::fs::create_dir_all(ext);
    }

    permissions::init_dirs(internal_data.clone(), external_data);
    if let Ok(mut slot) = global_android_app().lock() {
        *slot = Some(app.clone());
    }

    install_jni_bridge();

    // Request runtime storage/media permissions on startup so shared Movies/DCIM/Download folders
    // are immediately accessible.
    permissions::ensure_storage_permission();

    let env_gpu = std::env::var("FILMCRAFT_ANDROID_GPU").ok();
    let gpu_plan = gpu_startup::prepare_startup(&internal_data, env_gpu.as_deref());

    let mut options = eframe::NativeOptions {
        wgpu_options: gpu_startup::configure(gpu_plan.backend),
        android_app: Some(app),
        ..eframe::NativeOptions::default()
    };
    options.viewport = options.viewport.with_title("FilmCraft");

    let run_result = eframe::run_native(
        "FilmCraft",
        options,
        Box::new(move |cc| Ok(Box::new(AndroidShell::create(cc, gpu_plan, None)))),
    );

    if let Err(err) = run_result {
        log::error!("eframe::run_native exited with error: {err}");
    }
}

#[cfg(target_os = "android")]
fn install_jni_bridge() {
    let bridge = PlatformBridge {
        request_permissions: Box::new(jni_request_storage_permissions),
        request_audio_permission: Box::new(jni_request_audio_permission),
        launch_saf_open: Box::new(jni_launch_saf_open),
        launch_saf_save: Box::new(jni_launch_saf_save),
        launch_saf_folder: Box::new(jni_launch_saf_folder),
        scan_media_file: Box::new(jni_scan_media_file),
    };
    file_dialog::set_platform_bridge(bridge);
}

#[cfg(target_os = "android")]
fn with_activity_env<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut jni::Env<'_>, jni::objects::JObject<'_>) -> Option<R>,
{
    let app = match global_android_app().lock() {
        Ok(guard) => guard.clone()?,
        Err(poisoned) => poisoned.into_inner().clone()?,
    };
    let vm_ptr = app.vm_as_ptr().cast::<jni::sys::JavaVM>();
    let activity_ptr = app.activity_as_ptr().cast::<jni::sys::_jobject>();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        return None;
    }

    // SAFETY: `vm_as_ptr` and `activity_as_ptr` are valid pointers provided by `AndroidApp` for the
    // lifetime of the running `NativeActivity`.
    unsafe {
        let vm = jni::JavaVM::from_raw(vm_ptr);
        let mut out = None;
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let activity = jni::objects::JObject::from_raw(env, activity_ptr);
            out = f(env, activity);
            Ok(())
        });
        out
    }
}

#[cfg(target_os = "android")]
fn jni_request_storage_permissions() -> bool {
    with_activity_env(|env, activity| {
        let val = env
            .call_method(activity, jni::jni_str!("ensureStoragePermissions"), jni::jni_sig!("()Z"), &[])
            .ok()?;
        val.z().ok()
    })
    .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn jni_request_audio_permission() -> bool {
    with_activity_env(|env, activity| {
        let val = env
            .call_method(activity, jni::jni_str!("ensureAudioPermission"), jni::jni_sig!("()Z"), &[])
            .ok()?;
        val.z().ok()
    })
    .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn jni_launch_saf_open(extensions: &[String], multi: bool) -> bool {
    let joined = extensions.join(",");
    with_activity_env(|env, activity| {
        let ext_str = env.new_string(&joined).ok()?;
        let val = env
            .call_method(
                activity,
                jni::jni_str!("openDocumentPicker"),
                jni::jni_sig!("(Ljava/lang/String;Z)Z"),
                &[
                    jni::objects::JValue::Object(&ext_str),
                    jni::objects::JValue::Bool(multi),
                ],
            )
            .ok()?;
        val.z().ok()
    })
    .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn jni_launch_saf_save(suggested_name: &str, mime_type: &str) -> bool {
    with_activity_env(|env, activity| {
        let name_str = env.new_string(suggested_name).ok()?;
        let mime_str = env.new_string(mime_type).ok()?;
        let val = env
            .call_method(
                activity,
                jni::jni_str!("createDocumentPicker"),
                jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Z"),
                &[
                    jni::objects::JValue::Object(&name_str),
                    jni::objects::JValue::Object(&mime_str),
                ],
            )
            .ok()?;
        val.z().ok()
    })
    .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn jni_launch_saf_folder() -> bool {
    with_activity_env(|env, activity| {
        let val = env
            .call_method(activity, jni::jni_str!("openFolderPicker"), jni::jni_sig!("()Z"), &[])
            .ok()?;
        val.z().ok()
    })
    .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn jni_scan_media_file(path: &Path) {
    let path_str = path.to_string_lossy();
    let _ = with_activity_env(|env, activity| {
        let jpath = env.new_string(path_str.as_ref()).ok()?;
        let _ = env.call_method(
            activity,
            jni::jni_str!("scanMediaFile"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[jni::objects::JValue::Object(&jpath)],
        );
        Some(())
    });
}

// ============================================================================
// JNI callbacks invoked by `ai.storyteller.filmcraft.MainActivity`
// ============================================================================

/// Called by `MainActivity.onRequestPermissionsResult` when the user responds to the runtime
/// storage/media permission dialog.
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeOnStoragePermissionResult`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeOnStoragePermissionResult(
    _env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    granted: jni::sys::jboolean,
) {
    let state = if granted != false {
        PermissionState::Granted
    } else {
        PermissionState::Denied
    };
    permissions::set_storage_permission_state(state);
}

/// Called by `MainActivity.onRequestPermissionsResult` when the user responds to the `RECORD_AUDIO`
/// permission dialog.
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeOnAudioPermissionResult`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeOnAudioPermissionResult(
    _env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    granted: jni::sys::jboolean,
) {
    let state = if granted != false {
        PermissionState::Granted
    } else {
        PermissionState::Denied
    };
    permissions::set_audio_record_permission_state(state);
}

/// Called by `MainActivity` when an external `ACTION_VIEW` / `ACTION_SEND` intent or SAF picker
/// resolves a direct filesystem path.
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeDeliverFilePath`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeDeliverFilePath(
    mut env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    path: jni::objects::JString<'_>,
) {
    let _ = env.with_env(|env| -> Result<(), jni::errors::Error> {
        if let Ok(rust_str) = path.try_to_string(env)
            && !rust_str.trim().is_empty()
        {
            file_dialog::push_outcome(DialogOutcome::OpenFiles(vec![PathBuf::from(rust_str)]));
        }
        Ok(())
    });
}

/// Called by `MainActivity` when the user picks a file via SAF (`ACTION_OPEN_DOCUMENT`) or an
/// external `content://` URI intent (`ACTION_VIEW`) and `MainActivity` reads the bytes.
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeDeliverSafOpenFile` and
// `MainActivity.nativeDeliverFile`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeDeliverSafOpenFile(
    mut env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    display_name: jni::objects::JString<'_>,
    data: jni::objects::JByteArray<'_>,
) {
    deliver_bytes_from_jni(&mut env, display_name, &data);
}

/// Alias JNI callback matching `MainActivity.nativeDeliverFile(String, byte[])`.
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeDeliverFile`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeDeliverFile(
    mut env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    display_name: jni::objects::JString<'_>,
    data: jni::objects::JByteArray<'_>,
) {
    deliver_bytes_from_jni(&mut env, display_name, &data);
}

#[cfg(target_os = "android")]
fn deliver_bytes_from_jni(
    env: &mut jni::EnvUnowned<'_>,
    display_name: jni::objects::JString<'_>,
    data: &jni::objects::JByteArray<'_>,
) {
    let _ = env.with_env(|env| -> Result<(), jni::errors::Error> {
        let name = display_name
            .try_to_string(env)
            .unwrap_or_else(|_| "imported_media.mp4".to_string());
        if let Ok(bytes) = env.convert_byte_array(data)
            && let Err(err) = services::deliver_saf_open_bytes(&name, &bytes)
        {
            log::error!("failed to stage SAF open file '{name}': {err}");
        }
        Ok(())
    });
}

/// Called by `MainActivity` when the user selects a destination via SAF (`ACTION_CREATE_DOCUMENT`).
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeDeliverSafSavePath`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeDeliverSafSavePath(
    mut env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    target_path: jni::objects::JString<'_>,
) {
    let _ = env.with_env(|env| -> Result<(), jni::errors::Error> {
        if let Ok(path_str) = target_path.try_to_string(env)
            && !path_str.trim().is_empty()
        {
            file_dialog::deliver_saf_save_path(PathBuf::from(path_str));
        }
        Ok(())
    });
}

/// Called by `MainActivity` when the user selects a directory via SAF
/// (`ACTION_OPEN_DOCUMENT_TREE`).
#[cfg(target_os = "android")]
// SAFETY: Standard JNI callback signature matching `MainActivity.nativeDeliverSafFolderPath`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_filmcraft_MainActivity_nativeDeliverSafFolderPath(
    mut env: jni::EnvUnowned<'_>,
    _thiz: jni::objects::JObject<'_>,
    folder_path: jni::objects::JString<'_>,
) {
    let _ = env.with_env(|env| -> Result<(), jni::errors::Error> {
        if let Ok(path_str) = folder_path.try_to_string(env)
            && !path_str.trim().is_empty()
        {
            file_dialog::deliver_saf_folder_path(PathBuf::from(path_str));
        }
        Ok(())
    });
}
