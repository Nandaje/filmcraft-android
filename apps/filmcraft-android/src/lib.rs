//! FilmCraft Android application crate (`libfilmcraft_android.so`).
//!
//! Provides the Android NativeActivity entrypoint (`android_main`), hardware-accelerated `wgpu`
//! Vulkan / OpenGL ES GPU setup (`gpu_startup`) with `filmcraft_gpu` real-time monitor and export
//! compositing enabled by default, Android runtime file & microphone permissions (`permissions`),
//! touch-friendly + Storage Access Framework (SAF) import/export file dialogs (`file_dialog`),
//! Android `cpal` playback (`audio`) and voice-over recording (`audio_in`), and the `eframe`
//! shell (`shell`) wrapping [`filmcraft_ui_egui::FilmcraftApp`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod android_entry;
pub mod audio;
pub mod audio_in;
pub mod file_dialog;
pub mod gpu_startup;
pub mod permissions;
pub mod services;
pub mod shell;

// Windows GNU (`x86_64-pc-windows-gnu`) refuses to link a `cdylib` that exports zero symbols when
// building workspace tests on the host. Export a harmless version query symbol on non-Android
// hosts so `cargo test --workspace` links cleanly across Linux, macOS, and Windows.
#[cfg(not(target_os = "android"))]
#[allow(unsafe_code)]
// SAFETY: Exporting a pure constant-returning extern "C" function has no soundness preconditions.
#[unsafe(no_mangle)]
pub extern "C" fn filmcraft_android_abi_version() -> u32 {
    1
}
