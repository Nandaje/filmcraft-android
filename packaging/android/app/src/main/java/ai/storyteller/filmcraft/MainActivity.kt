package ai.storyteller.filmcraft

import android.Manifest
import android.app.Activity
import android.app.NativeActivity
import android.content.Intent
import android.content.pm.PackageManager
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import java.io.File
import java.io.FileOutputStream

/**
 * Android host Activity for FilmCraft (`libfilmcraft_android.so`).
 *
 * Bridges:
 * 1. Runtime storage, media, and microphone permissions (`READ_MEDIA_VIDEO`, `READ_MEDIA_AUDIO`,
 *    `READ_MEDIA_IMAGES`, `READ_EXTERNAL_STORAGE`, `WRITE_EXTERNAL_STORAGE`, `RECORD_AUDIO`) for
 *    importing and exporting video, audio, still images, captions, LUTs, and `.fcproj` projects.
 * 2. Storage Access Framework (SAF) system pickers (`ACTION_OPEN_DOCUMENT`,
 *    `ACTION_CREATE_DOCUMENT`, `ACTION_OPEN_DOCUMENT_TREE`).
 * 3. Incoming `ACTION_VIEW` / `ACTION_EDIT` / `ACTION_SEND` file intents.
 * 4. `MediaScannerConnection` indexing so exported MP4/MOV/WAV/etc. files appear immediately in
 *    Android Gallery, Movies, and Files apps.
 */
class MainActivity : NativeActivity() {

    companion object {
        private const val REQ_STORAGE_PERMISSIONS = 1001
        private const val REQ_AUDIO_PERMISSION = 1002
        private const val REQ_SAF_OPEN = 2001
        private const val REQ_SAF_SAVE = 2002
        private const val REQ_SAF_FOLDER = 2003

        init {
            System.loadLibrary("filmcraft_android")
        }
    }

    // JNI callbacks implemented in `apps/filmcraft-android/src/android_entry.rs`
    private external fun nativeOnStoragePermissionResult(granted: Boolean)
    private external fun nativeOnAudioPermissionResult(granted: Boolean)
    private external fun nativeDeliverFilePath(path: String)
    private external fun nativeDeliverSafOpenFile(displayName: String, data: ByteArray)
    private external fun nativeDeliverFile(displayName: String, data: ByteArray)
    private external fun nativeDeliverSafSavePath(targetPath: String)
    private external fun nativeDeliverSafFolderPath(folderPath: String)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        ensureStoragePermissions()
        handleIncomingIntent(intent)
    }

    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        if (intent != null) {
            handleIncomingIntent(intent)
        }
    }

    /**
     * Request runtime storage/media permissions appropriate for the running Android API level.
     * Returns `true` if permissions are already granted, or `false` if a dialog was launched.
     */
    fun ensureStoragePermissions(): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && Environment.isExternalStorageManager()) {
            nativeOnStoragePermissionResult(true)
            return true
        }

        val needed = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            for (perm in listOf(
                Manifest.permission.READ_MEDIA_VIDEO,
                Manifest.permission.READ_MEDIA_AUDIO,
                Manifest.permission.READ_MEDIA_IMAGES
            )) {
                if (checkSelfPermission(perm) != PackageManager.PERMISSION_GRANTED) {
                    needed.add(perm)
                }
            }
        } else {
            if (checkSelfPermission(Manifest.permission.READ_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                needed.add(Manifest.permission.READ_EXTERNAL_STORAGE)
            }
            if (Build.VERSION.SDK_INT <= Build.VERSION_CODES.Q &&
                checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED
            ) {
                needed.add(Manifest.permission.WRITE_EXTERNAL_STORAGE)
            }
        }

        if (needed.isEmpty()) {
            nativeOnStoragePermissionResult(true)
            return true
        }

        requestPermissions(needed.toTypedArray(), REQ_STORAGE_PERMISSIONS)
        return false
    }

    /**
     * Request `RECORD_AUDIO` runtime permission for timeline voice-over recording.
     */
    fun ensureAudioPermission(): Boolean {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
            nativeOnAudioPermissionResult(true)
            return true
        }
        requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), REQ_AUDIO_PERMISSION)
        return false
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQ_STORAGE_PERMISSIONS) {
            val granted = grantResults.isNotEmpty() && grantResults.any { it == PackageManager.PERMISSION_GRANTED }
            nativeOnStoragePermissionResult(granted)
        } else if (requestCode == REQ_AUDIO_PERMISSION) {
            val granted = grantResults.isNotEmpty() && grantResults.all { it == PackageManager.PERMISSION_GRANTED }
            nativeOnAudioPermissionResult(granted)
        }
    }

    /**
     * Launch the Android Storage Access Framework (`ACTION_OPEN_DOCUMENT`) system picker.
     */
    fun openDocumentPicker(extensionsCsv: String, multi: Boolean): Boolean {
        return try {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "*/*"
                putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multi)
                if (extensionsCsv.isNotBlank()) {
                    putExtra(
                        Intent.EXTRA_MIME_TYPES,
                        arrayOf("video/*", "audio/*", "image/*", "application/json", "application/octet-stream", "text/*")
                    )
                }
            }
            startActivityForResult(intent, REQ_SAF_OPEN)
            true
        } catch (_: Throwable) {
            false
        }
    }

    /**
     * Launch the Android Storage Access Framework (`ACTION_CREATE_DOCUMENT`) system save picker.
     */
    fun createDocumentPicker(suggestedName: String, mimeType: String): Boolean {
        return try {
            val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = mimeType.ifBlank { "video/mp4" }
                putExtra(Intent.EXTRA_TITLE, suggestedName.ifBlank { "Export.mp4" })
            }
            startActivityForResult(intent, REQ_SAF_SAVE)
            true
        } catch (_: Throwable) {
            false
        }
    }

    /**
     * Launch the Android Storage Access Framework (`ACTION_OPEN_DOCUMENT_TREE`) folder picker.
     */
    fun openFolderPicker(): Boolean {
        return try {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
            startActivityForResult(intent, REQ_SAF_FOLDER)
            true
        } catch (_: Throwable) {
            false
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != Activity.RESULT_OK || data == null) {
            return
        }
        when (requestCode) {
            REQ_SAF_OPEN -> {
                val clip = data.clipData
                if (clip != null && clip.itemCount > 0) {
                    for (i in 0 until clip.itemCount) {
                        deliverContentUriToRust(clip.getItemAt(i).uri)
                    }
                } else {
                    data.data?.let { deliverContentUriToRust(it) }
                }
            }
            REQ_SAF_SAVE -> {
                val uri = data.data ?: return
                val directFile = resolveDirectFileFromUri(uri)
                if (directFile != null) {
                    nativeDeliverSafSavePath(directFile.absolutePath)
                } else {
                    val name = queryDisplayName(uri) ?: "Export.mp4"
                    val baseDir = getExternalFilesDir(Environment.DIRECTORY_MOVIES)
                        ?: getExternalFilesDir(null)
                        ?: filesDir
                    val exportsDir = File(baseDir, "Exports").apply { mkdirs() }
                    val target = File(exportsDir, name)
                    nativeDeliverSafSavePath(target.absolutePath)
                }
            }
            REQ_SAF_FOLDER -> {
                val uri = data.data ?: return
                val folder = resolveDirectFolderFromTreeUri(uri)
                    ?: File(getExternalFilesDir(null) ?: filesDir, "Exports").apply { mkdirs() }
                nativeDeliverSafFolderPath(folder.absolutePath)
            }
        }
    }

    /**
     * Ask Android's MediaScannerConnection to index a saved/exported media file so it is
     * immediately visible in Android Gallery, Movies, Music, and the Files app.
     */
    fun scanMediaFile(path: String) {
        try {
            MediaScannerConnection.scanFile(this, arrayOf(path), null, null)
        } catch (_: Throwable) {
            // Ignore scanner errors on stripped-down Android builds.
        }
    }

    private fun handleIncomingIntent(intent: Intent) {
        when (intent.action) {
            Intent.ACTION_VIEW, Intent.ACTION_EDIT -> {
                intent.data?.let { uri ->
                    if (uri.scheme == "file") {
                        uri.path?.let { nativeDeliverFilePath(it) }
                    } else {
                        deliverContentUriToRust(uri)
                    }
                }
            }
            Intent.ACTION_SEND -> {
                @Suppress("DEPRECATION")
                val streamUri = intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM) ?: intent.data
                streamUri?.let { deliverContentUriToRust(it) }
            }
        }
    }

    private fun deliverContentUriToRust(uri: Uri) {
        try {
            // If we can resolve a direct readable POSIX path (e.g., for large video files on shared
            // storage), deliver the path directly so FilmCraft doesn't have to copy gigabytes into
            // RAM.
            val direct = resolveDirectFileFromUri(uri)
            if (direct != null && direct.canRead()) {
                nativeDeliverFilePath(direct.absolutePath)
                return
            }

            val displayName = queryDisplayName(uri) ?: "imported_media.mp4"
            // Stage via a stream copy into `filesDir/imports/<displayName>` for large files, or
            // deliver bytes if small.
            val importsDir = File(filesDir, "imports").apply { mkdirs() }
            val safeName = displayName.replace(Regex("[^A-Za-z0-9._ -]"), "_").ifBlank { "imported_media.mp4" }
            val stagedFile = File(importsDir, safeName)
            contentResolver.openInputStream(uri)?.use { input ->
                FileOutputStream(stagedFile).use { output ->
                    input.copyTo(output)
                }
            }
            if (stagedFile.exists() && stagedFile.length() > 0) {
                nativeDeliverFilePath(stagedFile.absolutePath)
            } else {
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() } ?: return
                nativeDeliverSafOpenFile(displayName, bytes)
            }
        } catch (_: Throwable) {
            // Ignore unreadable URIs.
        }
    }

    private fun queryDisplayName(uri: Uri): String? {
        return try {
            contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (idx >= 0) cursor.getString(idx) else null
                } else null
            }
        } catch (_: Throwable) {
            null
        }
    }

    private fun resolveDirectFileFromUri(uri: Uri): File? {
        return try {
            if (DocumentsContract.isDocumentUri(this, uri) &&
                uri.authority == "com.android.externalstorage.documents"
            ) {
                val docId = DocumentsContract.getDocumentId(uri)
                val parts = docId.split(":", limit = 2)
                if (parts.size == 2 && parts[0].equals("primary", ignoreCase = true)) {
                    val candidate = File(Environment.getExternalStorageDirectory(), parts[1])
                    if (candidate.exists() || candidate.parentFile?.canWrite() == true) {
                        return candidate
                    }
                }
            }
            null
        } catch (_: Throwable) {
            null
        }
    }

    private fun resolveDirectFolderFromTreeUri(uri: Uri): File? {
        return try {
            val treeId = DocumentsContract.getTreeDocumentId(uri)
            val parts = treeId.split(":", limit = 2)
            if (parts.size == 2 && parts[0].equals("primary", ignoreCase = true)) {
                val candidate = File(Environment.getExternalStorageDirectory(), parts[1])
                if (candidate.exists() || candidate.mkdirs()) {
                    return candidate
                }
            }
            null
        } catch (_: Throwable) {
            null
        }
    }
}
