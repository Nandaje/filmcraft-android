# FilmCraft on Android

This document covers the architecture, hardware GPU acceleration pipeline, file import/export permissions, and build instructions for the Android port of **FilmCraft** (`apps/filmcraft-android` and `packaging/android`).

---

## 1. Architecture Overview

| Component | Location | Responsibility |
|---|---|---|
| `filmcraft-android` crate | `apps/filmcraft-android/` | NativeActivity entrypoint (`libfilmcraft_android.so`), `eframe`/`wgpu` GPU initialization, `cpal` audio I/O, runtime permissions, touch file browser modal, and `AndroidServices` |
| `gpu_startup` | `apps/filmcraft-android/src/gpu_startup.rs` | Hardware GPU acceleration (`Vulkan` primary, `GLES` fallback, `HighPerformance`), unlocked hardware texture/compute limits for `filmcraft_gpu`, and crash-recovery sentinel (`gpu-starting.json`) |
| `permissions` | `apps/filmcraft-android/src/permissions.rs` | Android runtime storage/media/audio permission state (`READ_MEDIA_VIDEO`, `READ_MEDIA_AUDIO`, `READ_MEDIA_IMAGES`, `READ_EXTERNAL_STORAGE`, `WRITE_EXTERNAL_STORAGE`, `MANAGE_EXTERNAL_STORAGE`, `RECORD_AUDIO`), `AndroidDirs` discovery, and scoped-storage fallback |
| `file_dialog` | `apps/filmcraft-android/src/file_dialog.rs` | Touch-friendly in-app `egui` file/save/folder browser modal + JNI bridge to Android's Storage Access Framework (SAF) system picker (`FileDialog` non-blocking channel integration) |
| `services` | `apps/filmcraft-android/src/services.rs` | `AndroidServices` (`filmcraft_engine::Services`) with atomic writes, Media Browser volume discovery (`Movies`, `DCIM`, `Downloads`, etc.), and `MediaScannerConnection` notifications |
| `audio` / `audio_in` | `apps/filmcraft-android/src/audio.rs`, `audio_in.rs` | Android AAudio / Oboe playback (`CpalOut`) and timeline voice-over recording (`CpalIn`) |
| `shell` | `apps/filmcraft-android/src/shell.rs` | `AndroidShell` wrapping `FilmcraftApp` with `set_wgpu`, `register_frame_renderer`, touch-twin event deduplication, and export path normalization |
| Android Gradle Project | `packaging/android/` | `AndroidManifest.xml`, `MainActivity.kt` JNI bridge, and `package.sh` build script |

---

## 2. Hardware GPU Acceleration (Enabled by Default)

FilmCraft on Android turns hardware GPU acceleration **ON by default** at both the Android OS window layer and the `wgpu` / `filmcraft_gpu` rendering & compositing layer:

1. **Android Manifest (`AndroidManifest.xml`)**:
   - `<application android:hardwareAccelerated="true" android:largeHeap="true">`
   - `<activity android:hardwareAccelerated="true">`
   - Declares `<uses-feature android:glEsVersion="0x00030000" android:required="true" />` and optional `<uses-feature android:name="android.hardware.vulkan.version" android:version="0x401000" />`.
2. **`wgpu` Backend Selection (`gpu_startup::configure`)**:
   - Requests `wgpu::Backends::VULKAN | wgpu::Backends::GL` with `wgpu::PowerPreference::HighPerformance`.
   - Clears `wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL` for driver compatibility.
   - Custom `native_adapter_selector` ranks hardware GPUs (Qualcomm Adreno, ARM Mali / Immortalis, Samsung Xclipse, Imagination PowerVR) ahead of CPU rasterizers and prefers Vulkan 1.1+ over OpenGL ES 3.0+.
   - Unlocks the hardware adapter's full texture limits (`max_texture_dimension_1d/2d/3d`), buffer sizes (`max_buffer_size`, `max_storage_buffer_binding_size`), and compute workgroup limits on `wgpu::DeviceDescriptor`.
3. **Real-Time Monitor & Export GPU Compositor (`shell::AndroidShell::create`)**:
   - Calls `app.set_wgpu(rs)` so the Program and Source monitors composite video tracks, blend modes, transitions, Lumetri color wheels/curves/HSL secondaries, and WGSL compute effects (`fx::FxStage`) on the mobile GPU.
   - Calls `filmcraft_export::register_frame_renderer` with `filmcraft_gpu::ExportRenderer` so video exports also composite on the GPU.
4. **Crash-Safe Startup Sentinel (`gpu-starting.json`)**:
   - Before initializing the GPU surface, `prepare_startup` writes `gpu-starting.json` in internal app storage.
   - Once the first frame completes (`AndroidShell::ui`), the sentinel is deleted.
   - If a buggy OEM driver crashes during surface creation, the next launch automatically steps down (`Auto/Vulkan` → `Gl` → `Cpu`).
   - Can also be overridden via `FILMCRAFT_ANDROID_GPU=vulkan|gl|cpu|auto`.

---

## 3. File Permissions for Import & Export

Android's storage model varies across API levels 26–34+. FilmCraft handles every generation:

### Declared Manifest Permissions
- `android.permission.READ_EXTERNAL_STORAGE` (`maxSdkVersion="32"`)
- `android.permission.WRITE_EXTERNAL_STORAGE` (`maxSdkVersion="29"`)
- `android.permission.READ_MEDIA_VIDEO` (API 33+)
- `android.permission.READ_MEDIA_AUDIO` (API 33+)
- `android.permission.READ_MEDIA_IMAGES` (API 33+)
- `android.permission.MANAGE_EXTERNAL_STORAGE` (API 30+ full NLE project & camera-card access)
- `android.permission.RECORD_AUDIO` (Timeline voice-over recording)
- `android:requestLegacyExternalStorage="true"` for Android 10 (API 29) compatibility

### Dual-Mode Import & Export Workflow
1. **In-App Touch File & Folder Browser Modal (`file_dialog::show_file_dialog`)**:
   - Integrated with `FilmcraftApp`'s non-blocking `HostHooks::run_dialog` channel so every menu command (`File ▸ Import…`, `File ▸ Open Project…`, `File ▸ Save`, `File ▸ Save As…`, `Export Media`, `Export EDL / XML / FCPXML / OTIO / ALE / CSV`, `Link Media`, `Attach Proxies`, `Export Frame`, LUT browse) opens the touch modal without blocking the render loop.
   - Quick-jump bookmarks for **Movies**, **Camera (DCIM)**, **Downloads**, **Pictures**, **Music**, **Documents**, **Shared Storage**, **App External**, **App Exports**, and **App Internal**.
   - Supports multi-file selection for batch clip import and format extension chips for exports.
   - FilmCraft's built-in **Media Browser** panel also lists all Android storage bookmarks under **Local Drives** via `AndroidServices::volumes()`.
2. **Android Storage Access Framework (SAF) System Picker**:
   - Tapping **📂 Android System File Picker (SAF)** inside the modal invokes `MainActivity.openDocumentPicker`, `createDocumentPicker`, or `openFolderPicker` (`ACTION_OPEN_DOCUMENT`, `ACTION_CREATE_DOCUMENT`, `ACTION_OPEN_DOCUMENT_TREE`).
   - Large video files on shared storage are resolved directly to their POSIX paths when readable, or streamed into `<filesDir>/imports/` without exhausting heap memory.
3. **Scoped-Storage Export Fallback & MediaScanner Indexing**:
   - `permissions::resolve_export_target` ensures exports always succeed: if a target path's parent directory is restricted by scoped storage, it falls back to public `Movies`, `Download`, or `<externalFilesDir>/Exports`.
   - When an export job or project save completes, `MainActivity.scanMediaFile` triggers `MediaScannerConnection.scanFile` so exported `.mp4`, `.mov`, `.wav`, etc. files immediately appear in the Android Gallery, Movies, and Files apps.

---

## 4. Building & Packaging the APK

### Prerequisites
1. Rust toolchain with Android targets:
   ```sh
   rustup target add aarch64-linux-android x86_64-linux-android
   ```
2. `cargo-ndk`:
   ```sh
   cargo install cargo-ndk --locked
   ```
3. Android SDK (API 34) and Android NDK (`ANDROID_NDK_HOME` set).

### Build Native Libraries & APK
```sh
packaging/android/package.sh
```
To compile and stage only the `.so` libraries into `packaging/android/app/src/main/jniLibs/` without invoking Gradle:
```sh
packaging/android/package.sh --skip-gradle
```

### Running Host Unit Tests
All Android permission resolution, GPU startup sentinel state machines, touch-twin deduplication, and file dialog filter/channel logic are unit-tested on the host:
```sh
cargo test -p filmcraft-android
```
