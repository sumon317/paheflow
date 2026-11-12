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
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
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
        checkFirstRunPermissionOnboarding()

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
        settings.loadsImagesAutomatically = true
        settings.blockNetworkImage = false
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            settings.mixedContentMode = WebSettings.MIXED_CONTENT_ALWAYS_ALLOW
        }
        CookieManager.getInstance().setAcceptThirdPartyCookies(webView, true)

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

            @JavascriptInterface
            fun downloadAndInstallUpdate(apkUrl: String) {
                runOnUiThread {
                    this@MainActivity.downloadAndInstallUpdate(apkUrl)
                }
            }

            @JavascriptInterface
            fun isAndroid(): Boolean {
                return true
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
        if (isFinishing || isDestroyed) return
        val existing = supportFragmentManager.findFragmentByTag(CloudflareBottomSheet.TAG)
        if (existing != null && existing.isAdded) {
            return
        }
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
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            ContextCompat.checkSelfPermission(this, Manifest.permission.READ_MEDIA_VIDEO) == PackageManager.PERMISSION_GRANTED || canWriteToAppStorage()
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED || canWriteToAppStorage()
        } else {
            true
        }
    }

    private fun canWriteToAppStorage(): Boolean {
        return try {
            val dir = getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: return false
            dir.exists() || dir.mkdirs()
        } catch (_: Exception) {
            false
        }
    }

    fun promptStoragePermission() {
        val permissions = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_MEDIA_VIDEO) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.READ_MEDIA_VIDEO)
            }
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_MEDIA_AUDIO) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.READ_MEDIA_AUDIO)
            }
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_MEDIA_IMAGES) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.READ_MEDIA_IMAGES)
            }
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.READ_EXTERNAL_STORAGE)
            }
            if (Build.VERSION.SDK_INT <= Build.VERSION_CODES.Q &&
                ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.WRITE_EXTERNAL_STORAGE)
            }
        }

        if (permissions.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, permissions.toTypedArray(), 102)
        } else {
            Toast.makeText(this, "Storage permission is already granted", Toast.LENGTH_SHORT).show()
            checkAndNotifyStoragePermission()
        }
    }

    private fun checkFirstRunPermissionOnboarding() {
        val prefs = getSharedPreferences("paheflow_prefs", MODE_PRIVATE)
        val prompted = prefs.getBoolean("first_run_storage_prompt_shown", false)
        if (!prompted) {
            prefs.edit().putBoolean("first_run_storage_prompt_shown", true).apply()
            if (!hasStoragePermission()) {
                showFirstRunPermissionDialog()
            }
        }
    }

    private fun showFirstRunPermissionDialog() {
        if (isFinishing || isDestroyed) return
        AlertDialog.Builder(this)
            .setTitle("Storage Permission")
            .setMessage("PaheFlow needs storage permission to save downloaded anime episodes to your device's Download folder. Would you like to grant permission now?")
            .setPositiveButton("Grant Permission") { _, _ ->
                promptStoragePermission()
            }
            .setNegativeButton("Later", null)
            .show()
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 102) {
            checkAndNotifyStoragePermission()
        }
    }

    fun checkAndNotifyStoragePermission() {
        val hasPerm = hasStoragePermission()
        webView.evaluateJavascript("if (typeof window.onStoragePermissionChanged === 'function') window.onStoragePermissionChanged($hasPerm);", null)
    }

    private var pendingInstallApk: File? = null

    override fun onResume() {
        super.onResume()
        checkAndNotifyStoragePermission()
        if (pendingInstallApk != null) {
            val apk = pendingInstallApk
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O || packageManager.canRequestPackageInstalls()) {
                pendingInstallApk = null
                if (apk != null && apk.exists()) {
                    installDownloadedApk(apk)
                }
            }
        }
    }

    fun downloadAndInstallUpdate(apkUrl: String) {
        lifecycleScope.launch(kotlinx.coroutines.Dispatchers.IO) {
            try {
                runOnUiThread {
                    webView.evaluateJavascript(
                        "if (typeof window.onUpdateDownloadProgress === 'function') window.onUpdateDownloadProgress(0, 'Connecting...');",
                        null
                    )
                }
                val cacheFolder = externalCacheDir ?: getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: cacheDir
                val destFile = File(cacheFolder, "paheflow-update.apk")
                if (destFile.exists()) {
                    destFile.delete()
                }

                var currentUrl = apkUrl
                var connection: java.net.HttpURLConnection? = null
                var redirects = 0
                while (redirects < 8) {
                    val urlObj = java.net.URL(currentUrl)
                    connection = urlObj.openConnection() as java.net.HttpURLConnection
                    connection.instanceFollowRedirects = false
                    connection.setRequestProperty("User-Agent", "PaheFlow-Updater")
                    connection.setRequestProperty("Accept-Encoding", "identity")
                    connection.connectTimeout = 15000
                    connection.readTimeout = 30000
                    val status = connection.responseCode
                    if (status == java.net.HttpURLConnection.HTTP_MOVED_PERM ||
                        status == java.net.HttpURLConnection.HTTP_MOVED_TEMP ||
                        status == 307 || status == 308
                    ) {
                        val location = connection.getHeaderField("Location") ?: break
                        currentUrl = if (location.startsWith("http")) location else java.net.URL(urlObj, location).toString()
                        redirects++
                        connection.disconnect()
                    } else {
                        break
                    }
                }

                if (connection == null || connection.responseCode != 200) {
                    val code = connection?.responseCode ?: -1
                    runOnUiThread {
                        Toast.makeText(this@MainActivity, "Failed to download update (HTTP $code)", Toast.LENGTH_LONG).show()
                        webView.evaluateJavascript(
                            "if (typeof window.onUpdateDownloadFailed === 'function') window.onUpdateDownloadFailed('HTTP $code');",
                            null
                        )
                    }
                    return@launch
                }

                val totalBytes = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                    connection.contentLengthLong
                } else {
                    connection.contentLength.toLong()
                }
                var downloadedBytes = 0L
                val buffer = ByteArray(32 * 1024)
                var lastPercent = -1

                connection.inputStream.use { input ->
                    destFile.outputStream().use { output ->
                        var read: Int
                        while (input.read(buffer).also { read = it } != -1) {
                            output.write(buffer, 0, read)
                            downloadedBytes += read
                            if (totalBytes > 0) {
                                val percent = ((downloadedBytes * 100) / totalBytes).toInt().coerceIn(0, 99)
                                if (percent != lastPercent) {
                                    lastPercent = percent
                                    runOnUiThread {
                                        webView.evaluateJavascript(
                                            "if (typeof window.onUpdateDownloadProgress === 'function') window.onUpdateDownloadProgress($percent, '$percent%');",
                                            null
                                        )
                                    }
                                }
                            } else {
                                val mb = downloadedBytes / (1024f * 1024f)
                                val text = String.format(java.util.Locale.US, "%.1f MB", mb)
                                val approxPercent = ((downloadedBytes * 100) / (25 * 1024 * 1024)).toInt().coerceIn(1, 95)
                                if (approxPercent != lastPercent) {
                                    lastPercent = approxPercent
                                    runOnUiThread {
                                        webView.evaluateJavascript(
                                            "if (typeof window.onUpdateDownloadProgress === 'function') window.onUpdateDownloadProgress($approxPercent, '$text');",
                                            null
                                        )
                                    }
                                }
                            }
                        }
                    }
                }

                runOnUiThread {
                    webView.evaluateJavascript(
                        "if (typeof window.onUpdateDownloadProgress === 'function') window.onUpdateDownloadProgress(100, 'Ready to install');",
                        null
                    )
                    installDownloadedApk(destFile)
                }
            } catch (e: Exception) {
                runOnUiThread {
                    Toast.makeText(this@MainActivity, "Download error: ${e.message}", Toast.LENGTH_LONG).show()
                    val escaped = e.message?.replace("'", "\\'") ?: "Unknown error"
                    webView.evaluateJavascript(
                        "if (typeof window.onUpdateDownloadFailed === 'function') window.onUpdateDownloadFailed('$escaped');",
                        null
                    )
                }
            }
        }
    }

    fun installDownloadedApk(apkFile: File) {
        if (!apkFile.exists() || apkFile.length() == 0L) {
            Toast.makeText(this, "Downloaded update is empty or missing", Toast.LENGTH_SHORT).show()
            return
        }

        if (!isApkSignedWithSameKey(apkFile)) {
            AlertDialog.Builder(this)
                .setTitle("Update Signature Mismatch")
                .setMessage(
                    "The downloaded update was not signed with the same key as the installed PaheFlow app, " +
                        "so it will not be installed.\n\n" +
                        "If you are upgrading from v1.1.0 or older, the app is now signed with a new key: " +
                        "uninstall the old version first, then install the new APK."
                )
                .setPositiveButton("OK", null)
                .show()
            return
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            if (!packageManager.canRequestPackageInstalls()) {
                pendingInstallApk = apkFile
                AlertDialog.Builder(this)
                    .setTitle("Allow App Installation")
                    .setMessage("To install the PaheFlow update, please allow installing unknown apps from PaheFlow in system Settings.")
                    .setPositiveButton("Open Settings") { _, _ ->
                        try {
                            val intent = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES).apply {
                                data = Uri.parse("package:$packageName")
                            }
                            startActivity(intent)
                        } catch (e: Exception) {
                            Toast.makeText(this, "Could not open settings: ${e.message}", Toast.LENGTH_SHORT).show()
                        }
                    }
                    .setNegativeButton("Cancel") { _, _ ->
                        pendingInstallApk = null
                    }
                    .show()
                return
            }
        }

        try {
            val apkUri = androidx.core.content.FileProvider.getUriForFile(
                this,
                "${packageName}.fileprovider",
                apkFile
            )
            val installIntent = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(apkUri, "application/vnd.android.package-archive")
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            startActivity(installIntent)
        } catch (e: Exception) {
            Toast.makeText(this, "Could not launch package installer: ${e.message}", Toast.LENGTH_LONG).show()
        }
    }

    private fun isApkSignedWithSameKey(apkFile: File): Boolean {
        return try {
            val archiveInfo = packageManager.getPackageArchiveInfo(
                apkFile.absolutePath,
                PackageManager.GET_SIGNATURES
            ) ?: return false
            val installedInfo = packageManager.getPackageInfo(
                packageName,
                PackageManager.GET_SIGNATURES
            )
            val archiveCerts = archiveInfo.signatures
                ?.map { it.toByteArray().sha256Hex() }
                ?.toSet()
                ?: return false
            val installedCerts = installedInfo.signatures
                ?.map { it.toByteArray().sha256Hex() }
                ?.toSet()
                ?: return false
            archiveCerts == installedCerts
        } catch (e: Exception) {
            false
        }
    }

    private fun ByteArray.sha256Hex(): String =
        java.security.MessageDigest.getInstance("SHA-256").digest(this)
            .joinToString("") { "%02x".format(it) }

    private fun ensureDirAndCheckPermission(dirPath: String) {
        try {
            File(dirPath).mkdirs()
        } catch (_: Exception) {}
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
