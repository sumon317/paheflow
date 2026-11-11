package com.animepahe.app

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.File
import java.net.Socket

class MainActivity : AppCompatActivity() {

    companion object {
        var currentInstance: java.lang.ref.WeakReference<MainActivity>? = null
    }

    private lateinit var webView: WebView
    private var serverPort: Int = 8765

    private val openDocumentTreeLauncher = registerForActivityResult(
        ActivityResultContracts.OpenDocumentTree()
    ) { uri: Uri? ->
        if (uri != null) {
            val path = resolveTreeUriToPath(uri)
            if (path != null) {
                ensureDirAndCheckPermission(path)
                notifyFolderPicked(path)
            } else {
                notifyFolderPicked(null)
            }
        } else {
            notifyFolderPicked(null)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        currentInstance = java.lang.ref.WeakReference(this)

        // Request notification permission on Android 13+
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 101)
            }
        }

        // Check storage directory and permissions on startup
        val standardDownload = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)?.absolutePath
            ?: "/storage/emulated/0/Download"
        val paheFlowDownload = "$standardDownload/PaheFlow"
        ensureDirAndCheckPermission(paheFlowDownload)

        // Start background service
        AnimePaheService.start(this)

        setupWebView()

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                webView.evaluateJavascript("""
                    (function() {
                        if (document.body.classList.contains("show-mobile-activity")) {
                            if (typeof window.toggleActivityPanel === "function") {
                                window.toggleActivityPanel(false);
                            } else {
                                document.body.classList.remove("show-mobile-activity");
                            }
                            return true;
                        }
                        return false;
                    })();
                """.trimIndent()) { result ->
                    if (result != "true") {
                        if (webView.canGoBack()) {
                            webView.goBack()
                        } else {
                            finish()
                        }
                    }
                }
            }
        })

        waitForServerAndLoad()
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun setupWebView() {
        val container = FrameLayout(this).apply {
            setBackgroundColor(0xFF0B0A10.toInt())
        }

        webView = WebView(this).apply {
            layoutParams = FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT
            )
            setBackgroundColor(0xFF0B0A10.toInt())
        }

        val settings = webView.settings
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.databaseEnabled = true
        settings.setSupportZoom(false)
        settings.builtInZoomControls = false
        settings.displayZoomControls = false
        settings.useWideViewPort = false
        settings.loadWithOverviewMode = false
        settings.textZoom = 100
        settings.allowFileAccess = false
        settings.allowContentAccess = false

        webView.overScrollMode = WebView.OVER_SCROLL_NEVER

        // Bridge for native Cloudflare solver and browser launch
        webView.addJavascriptInterface(object {
            @JavascriptInterface
            fun openCloudflareSolver() {
                runOnUiThread {
                    showCloudflareSolver()
                }
            }

            @JavascriptInterface
            fun openInFirefox() {
                runOnUiThread {
                    try {
                        val intent = Intent(Intent.ACTION_VIEW, Uri.parse("https://animepahe.pw/")).apply {
                            setPackage("org.mozilla.firefox")
                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                        }
                        startActivity(intent)
                    } catch (_: Exception) {
                        try {
                            val intent = Intent(Intent.ACTION_VIEW, Uri.parse("https://animepahe.pw/")).apply {
                                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                            }
                            startActivity(intent)
                        } catch (e: Exception) {
                            Toast.makeText(this@MainActivity, "Could not open browser: ${e.message}", Toast.LENGTH_SHORT).show()
                        }
                    }
                }
            }

            @JavascriptInterface
            fun openFolderPicker() {
                runOnUiThread {
                    showFolderPickerDialog()
                }
            }

            @JavascriptInterface
            fun hasStoragePermission(): Boolean {
                return this@MainActivity.hasStoragePermission()
            }

            @JavascriptInterface
            fun requestStoragePermission() {
                runOnUiThread {
                    promptStoragePermission()
                }
            }

            @JavascriptInterface
            fun shutdownApp() {
                runOnUiThread {
                    shutdown()
                }
            }
        }, "AndroidBridge")

        webView.webViewClient = object : WebViewClient() {
            override fun onPageStarted(view: WebView?, url: String?, favicon: Bitmap?) {
                super.onPageStarted(view, url, favicon)
            }

            override fun onPageFinished(view: WebView?, url: String?) {
                super.onPageFinished(view, url)
                // Hook frontend solver buttons to open native Android bottom sheet solver
                view?.evaluateJavascript("""
                    (function() {
                        window.openCfSolverModal = function() {
                            if (window.AndroidBridge && window.AndroidBridge.openCloudflareSolver) {
                                window.AndroidBridge.openCloudflareSolver();
                            }
                        };
                    })();
                """.trimIndent(), null)
                checkAndNotifyStoragePermission()
            }

            override fun onReceivedError(
                view: WebView?,
                request: WebResourceRequest?,
                error: WebResourceError?
            ) {
                super.onReceivedError(view, request, error)
            }
        }

        webView.webChromeClient = WebChromeClient()

        container.addView(webView)
        setContentView(container)
    }

    private fun waitForServerAndLoad() {
        lifecycleScope.launch {
            var ready = false
            var attempts = 0
            while (!ready && attempts < 30) {
                delay(300)
                attempts++
                val p = NativeEngine.getServerPort()
                if (p > 0) {
                    serverPort = p
                    ready = isPortOpen("127.0.0.1", p)
                }
            }

            if (ready) {
                webView.loadUrl("http://127.0.0.1:$serverPort/")
            } else {
                Toast.makeText(this@MainActivity, "Server starting up, retrying…", Toast.LENGTH_SHORT).show()
                delay(1000)
                val p = NativeEngine.getServerPort()
                if (p > 0) webView.loadUrl("http://127.0.0.1:$p/")
            }
        }
    }

    private fun isPortOpen(host: String, port: Int): Boolean {
        return try {
            Socket(host, port).use { true }
        } catch (_: Exception) {
            false
        }
    }

    fun showCloudflareSolver() {
        val sheet = CloudflareBottomSheet.newInstance {
            // Dispatch event to app without reloading page to preserve user selections
            webView.evaluateJavascript(
                "window.dispatchEvent(new CustomEvent('animepahe-cookie-synced'));",
                null
            )
        }
        sheet.show(supportFragmentManager, CloudflareBottomSheet.TAG)
    }

    private fun showFolderPickerDialog() {
        val standardDownload = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)?.absolutePath
            ?: "/storage/emulated/0/Download"
        val paheFlowDownload = "$standardDownload/PaheFlow"
        val moviesDir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_MOVIES)?.absolutePath
            ?: "/storage/emulated/0/Movies"

        val options = arrayOf(
            "📁 PaheFlow ($paheFlowDownload) [Default]",
            "📥 Downloads root ($standardDownload)",
            "🎬 Movies ($moviesDir)",
            "📂 Browse other folders (System Picker)…"
        )

        AlertDialog.Builder(this)
            .setTitle("Choose Download Folder")
            .setItems(options) { _, which ->
                when (which) {
                    0 -> {
                        ensureDirAndCheckPermission(paheFlowDownload)
                        notifyFolderPicked(paheFlowDownload)
                    }
                    1 -> {
                        ensureDirAndCheckPermission(standardDownload)
                        notifyFolderPicked(standardDownload)
                    }
                    2 -> {
                        ensureDirAndCheckPermission(moviesDir)
                        notifyFolderPicked(moviesDir)
                    }
                    3 -> {
                        ensureDirAndCheckPermission(paheFlowDownload)
                        try {
                            openDocumentTreeLauncher.launch(null)
                        } catch (e: Exception) {
                            Toast.makeText(this, "Could not open system picker: ${e.message}", Toast.LENGTH_SHORT).show()
                            notifyFolderPicked(null)
                        }
                    }
                }
            }
            .setNegativeButton("Cancel") { _, _ ->
                notifyFolderPicked(null)
            }
            .setOnCancelListener {
                notifyFolderPicked(null)
            }
            .show()
    }

    fun hasStoragePermission(): Boolean {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            Environment.isExternalStorageManager()
        } else {
            ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED
        }
    }

    fun promptStoragePermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            if (!Environment.isExternalStorageManager()) {
                try {
                    val intent = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION).apply {
                        data = Uri.parse("package:$packageName")
                    }
                    startActivity(intent)
                    Toast.makeText(this, "Please allow 'All files access' for PaheFlow to save downloads to storage", Toast.LENGTH_LONG).show()
                } catch (_: Exception) {
                    try {
                        val intent = Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)
                        startActivity(intent)
                    } catch (e: Exception) {
                        Toast.makeText(this, "Please enable storage access in Settings: ${e.message}", Toast.LENGTH_LONG).show()
                    }
                }
            } else {
                Toast.makeText(this, "Storage permission is already granted", Toast.LENGTH_SHORT).show()
            }
        } else {
            val permissions = mutableListOf<String>()
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.READ_EXTERNAL_STORAGE)
            }
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.WRITE_EXTERNAL_STORAGE)
            }
            if (permissions.isNotEmpty()) {
                ActivityCompat.requestPermissions(this, permissions.toTypedArray(), 102)
            }
        }
    }

    fun checkAndNotifyStoragePermission() {
        val hasPerm = hasStoragePermission()
        webView.evaluateJavascript("if (typeof window.onStoragePermissionChanged === 'function') window.onStoragePermissionChanged($hasPerm);", null)
    }

    override fun onResume() {
        super.onResume()
        checkAndNotifyStoragePermission()
    }

    private fun ensureDirAndCheckPermission(dirPath: String) {
        try {
            File(dirPath).mkdirs()
        } catch (_: Exception) {}

        if (!hasStoragePermission()) {
            promptStoragePermission()
        }
    }

    private fun resolveTreeUriToPath(uri: Uri): String? {
        try {
            contentResolver.takePersistableUriPermission(
                uri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            )
        } catch (_: Exception) {}

        return try {
            val docId = DocumentsContract.getTreeDocumentId(uri) ?: return null
            val parts = docId.split(":", limit = 2)
            val type = parts[0]
            val relativePath = if (parts.size > 1) parts[1] else ""

            if ("primary".equals(type, ignoreCase = true)) {
                val extDir = Environment.getExternalStorageDirectory().absolutePath
                if (relativePath.isEmpty()) extDir else "$extDir/$relativePath"
            } else if (docId.startsWith("raw:")) {
                docId.substring(4)
            } else {
                "/storage/$type/$relativePath".trimEnd('/')
            }
        } catch (_: Exception) {
            null
        }
    }

    private fun notifyFolderPicked(path: String?) {
        runOnUiThread {
            val escaped = path?.replace("\\", "\\\\")?.replace("\"", "\\\"")?.replace("'", "\\'")
            val js = if (escaped != null) {
                "if (typeof window.onFolderPicked === 'function') { window.onFolderPicked('$escaped'); }"
            } else {
                "if (typeof window.onFolderPicked === 'function') { window.onFolderPicked(null); }"
            }
            webView.evaluateJavascript(js, null)
        }
    }

    fun shutdown() {
        AnimePaheService.stop(this)
        finishAndRemoveTask()
        android.os.Process.killProcess(android.os.Process.myPid())
    }

    override fun onDestroy() {
        if (currentInstance?.get() == this) {
            currentInstance = null
        }
        webView.destroy()
        super.onDestroy()
    }
}
