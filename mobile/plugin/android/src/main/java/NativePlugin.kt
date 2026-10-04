package com.popupmessenger.pmnative

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.view.HapticFeedbackConstants
import android.webkit.MimeTypeMap
import android.webkit.WebView
import androidx.core.content.FileProvider
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File

/** Declared in this library's manifest; a subclass so it never clashes with another FileProvider. */
class PmFileProvider : FileProvider()

@InvokeArg
class PathArgs {
    var path: String = ""
}

@TauriPlugin
class NativePlugin(private val activity: Activity) : Plugin(activity) {

    /**
     * Android 15+ draws apps edge to edge: the WebView reports no safe-area insets and no
     * longer shrinks for the keyboard. Hand the system bar, cutout and keyboard sizes (in
     * CSS pixels) to the page whenever they change; app.js lays out with them.
     */
    private var webView: WebView? = null

    override fun load(webView: WebView) {
        this.webView = webView
        ViewCompat.setOnApplyWindowInsetsListener(webView) { v, insets ->
            val d = activity.resources.displayMetrics.density
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
            val js = "window.__pmInsets && window.__pmInsets(%.1f,%.1f,%.1f,%.1f,%.1f)".format(
                java.util.Locale.ROOT, bars.top / d, bars.bottom / d, bars.left / d, bars.right / d, ime.bottom / d)
            webView.post { webView.evaluateJavascript(js, null) }
            ViewCompat.onApplyWindowInsets(v, insets)
        }
        webView.post { ViewCompat.requestApplyInsets(webView) }
    }

    private fun file(invoke: Invoke): File? {
        val f = File(invoke.parseArgs(PathArgs::class.java).path)
        if (!f.isFile) {
            invoke.reject("파일이 없습니다")
            return null
        }
        return f
    }

    /** A content:// URI other apps may read; files outside the provider's roots are copied to the cache first. */
    private fun uriFor(f: File): Uri {
        val authority = activity.packageName + ".pmfiles"
        return try {
            FileProvider.getUriForFile(activity, authority, f)
        } catch (e: IllegalArgumentException) {
            val dir = File(activity.cacheDir, "share").apply { mkdirs() }
            val copy = File(dir, f.name)
            f.copyTo(copy, overwrite = true)
            FileProvider.getUriForFile(activity, authority, copy)
        }
    }

    private fun mimeOf(f: File): String =
        MimeTypeMap.getSingleton().getMimeTypeFromExtension(f.extension.lowercase()) ?: "application/octet-stream"

    @Command
    fun open(invoke: Invoke) {
        val f = file(invoke) ?: return
        activity.runOnUiThread {
            try {
                val uri = uriFor(f)
                val view = Intent(Intent.ACTION_VIEW)
                    .setDataAndType(uri, mimeOf(f))
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                activity.startActivity(view)
                invoke.resolve()
            } catch (e: ActivityNotFoundException) {
                invoke.reject("이 파일을 열 수 있는 앱이 없습니다. 공유를 사용하세요")
            } catch (e: Exception) {
                invoke.reject(e.message ?: "파일을 열 수 없습니다")
            }
        }
    }

    @Command
    fun share(invoke: Invoke) {
        val f = file(invoke) ?: return
        activity.runOnUiThread {
            try {
                val uri = uriFor(f)
                val send = Intent(Intent.ACTION_SEND)
                    .setType(mimeOf(f))
                    .putExtra(Intent.EXTRA_STREAM, uri)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                send.clipData = ClipData.newRawUri(f.name, uri)
                activity.startActivity(Intent.createChooser(send, f.name))
                invoke.resolve()
            } catch (e: Exception) {
                invoke.reject(e.message ?: "공유할 수 없습니다")
            }
        }
    }

    /** Copies into the public Download/PopupMessenger folder (Android 10+, no permission needed). */
    @Command
    fun save(invoke: Invoke) {
        val f = file(invoke) ?: return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            invoke.reject("이 Android 버전에서는 공유 > 파일에 저장을 사용하세요")
            return
        }
        Thread {
            try {
                val folder = Environment.DIRECTORY_DOWNLOADS + "/PopupMessenger"
                val values = ContentValues().apply {
                    put(MediaStore.MediaColumns.DISPLAY_NAME, f.name)
                    put(MediaStore.MediaColumns.MIME_TYPE, mimeOf(f))
                    put(MediaStore.MediaColumns.RELATIVE_PATH, folder)
                    put(MediaStore.MediaColumns.IS_PENDING, 1)
                }
                val resolver = activity.contentResolver
                val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: throw Exception("저장할 수 없습니다")
                try {
                    resolver.openOutputStream(uri)?.use { out -> f.inputStream().use { it.copyTo(out) } }
                        ?: throw Exception("저장할 수 없습니다")
                    values.clear()
                    values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                    resolver.update(uri, values, null, null)
                } catch (e: Exception) {
                    resolver.delete(uri, null, null)
                    throw e
                }
                val ret = JSObject()
                ret.put("saved", "$folder/${f.name}")
                invoke.resolve(ret)
            } catch (e: Exception) {
                invoke.reject(e.message ?: "저장할 수 없습니다")
            }
        }.start()
    }

    /** Sends the insets again (the page may have loaded after the first report). */
    @Command
    fun insets(invoke: Invoke) {
        activity.runOnUiThread {
            webView?.let { ViewCompat.requestApplyInsets(it) }
            invoke.resolve()
        }
    }

    @Command
    fun haptic(invoke: Invoke) {
        activity.runOnUiThread {
            val flag = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) HapticFeedbackConstants.CONFIRM
                       else HapticFeedbackConstants.LONG_PRESS
            activity.window?.decorView?.performHapticFeedback(flag)
            invoke.resolve()
        }
    }
}
