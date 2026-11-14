package com.animepahe.app

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.content.ContentValues
import android.net.Uri
import android.provider.MediaStore
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.DocumentsContract
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.FrameLayout
import android.widget.Toast
import android.view.LayoutInflater
import android.view.View
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.File

class MainActivity : AppCompatActivity() {

    companion object {
        var currentInstance: java.lang.ref.WeakReference<MainActivity>? = null
    }

    private lateinit var webView: WebView
    private var serverPort: Int = 8765
    private var lastBackPressTime: Long = 0L

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
                webView.evaluateJavascript("window.handleBackPress();") { result ->
                    val cleaned = result?.replace("\"", "") ?: ""
                    when (cleaned) {
                        "handled" -> {
                            // JS handled it (closed panel, went back in tab history, closed modal)
                            // Reset double-tap timer since user navigated
                            lastBackPressTime = 0L
                        }
                        "webview" -> {
                            // WebView history - but don't use it for main app navigation to avoid blank screens
                            // Instead, treat as "no JS history" and trigger double-tap exit logic
                            handleDoubleTapExit()
                        }
                        else -> {
                            // Default: trigger double-tap exit logic
                            handleDoubleTapExit()
                        }
                    }
                }
            }
        })

        waitForServerAndLoad()
    }

    private fun handleDoubleTapExit() {
        val currentTime = System.currentTimeMillis()
        if (currentTime - lastBackPressTime < 2000) {
            // Double tap detected - show themed exit confirmation
            showThemedExitConfirmationDialog()
        } else {
            // First tap - show toast
            lastBackPressTime = currentTime
            runOnUiThread {
                Toast.makeText(this@MainActivity, "Press back again to exit", Toast.LENGTH_SHORT).show()
            }
        }
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
            settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
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
            fun downloadLatestUpdate(apkUrl: String) {
                runOnUiThread {
                    this@MainActivity.downloadLatestUpdate(apkUrl)
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

        webView.webChromeClient = object : WebChromeClient() {
            override fun onConsoleMessage(consoleMessage: android.webkit.ConsoleMessage?): Boolean {
                android.util.Log.d("WebViewConsole", "${consoleMessage?.message()} -- From line ${consoleMessage?.lineNumber()} of ${consoleMessage?.sourceId()}")
                return true
            }

            override fun onJsPrompt(
                view: WebView?,
                url: String?,
                message: String?,
                defaultValue: String?,
                result: android.webkit.JsPromptResult?
            ): Boolean {
                val input = android.widget.EditText(this@MainActivity).apply {
                    setText(defaultValue ?: "")
                    selectAll()
                }
                androidx.appcompat.app.AlertDialog.Builder(this@MainActivity)
                    .setTitle(message ?: "Enter value")
                    .setView(input)
                    .setPositiveButton("OK") { _, _ ->
                        result?.confirm(input.text.toString())
                    }
                    .setNegativeButton("Cancel") { _, _ ->
                        result?.cancel()
                    }
                    .setOnCancelListener {
                        result?.cancel()
                    }
                    .show()
                return true
            }
        }

        ViewCompat.setOnApplyWindowInsetsListener(container) { v, windowInsets ->
            val insets = windowInsets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            v.setPadding(insets.left, insets.top, insets.right, insets.bottom)
            windowInsets
        }

        container.addView(webView)
        setContentView(container)
    }

    private fun waitForServerAndLoad() {
        lifecycleScope.launch {
            val port = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                val app = AnimePaheApp.instance
                app.awaitAssetsReady()

                var p = NativeEngine.startServer(
                    preferredPort = 8765,
                    resDir = app.getResourceDir().absolutePath,
                    dataDir = app.getAppDataDir().absolutePath,
                    extDownloadDir = app.getExternalDownloadDir().absolutePath,
                    cacheDir = app.getAppCacheDir().absolutePath
                )
                var attempts = 0
                while (p <= 0 && attempts < 40) {
                    delay(50)
                    attempts++
                    p = NativeEngine.getServerPort()
                }
                p
            }

            if (port > 0) {
                serverPort = port
                webView.loadUrl("http://127.0.0.1:$serverPort/")
            } else {
                Toast.makeText(this@MainActivity, "Server starting up, retrying…", Toast.LENGTH_SHORT).show()
                delay(300)
                val p = NativeEngine.getServerPort()
                if (p > 0) {
                    serverPort = p
                    webView.loadUrl("http://127.0.0.1:$p/")
                }
            }
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

    private fun showThemedExitConfirmationDialog() {
        val dialogView = layoutInflater.inflate(R.layout.dialog_exit_confirmation, null)
        val dialog = AlertDialog.Builder(this)
            .setView(dialogView)
            .create()

        dialogView.findViewById<android.widget.TextView>(R.id.dialogTitle).text = "Exit PaheFlow?"
        dialogView.findViewById<android.widget.TextView>(R.id.dialogMessage).text = "Are you sure you want to close the app?"
        
        dialogView.findViewById<android.widget.Button>(R.id.btnStay).setOnClickListener { dialog.dismiss() }
        dialogView.findViewById<android.widget.Button>(R.id.btnExit).setOnClickListener {
            dialog.dismiss()
            if (AnimePaheService.hasActiveDownload) {
                Toast.makeText(this, "Downloads continuing in background", Toast.LENGTH_SHORT).show()
            } else {
                AnimePaheService.stopIfIdle(applicationContext)
            }
            finish()
        }

        dialog.setOnShowListener { d ->
            val alertDialog = d as AlertDialog
            alertDialog.getButton(AlertDialog.BUTTON_POSITIVE)?.setTextColor(resources.getColor(R.color.accent, null))
            alertDialog.getButton(AlertDialog.BUTTON_NEGATIVE)?.setTextColor(resources.getColor(R.color.text_primary, null))
        }

        dialog.show()
    }

    // Kept for compatibility but now uses themed version
    @Suppress("UNUSED_PARAMETER")
    private fun showExitConfirmationDialog() {
        showThemedExitConfirmationDialog()
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

    override fun onResume() {
        super.onResume()
        checkAndNotifyStoragePermission()
    }

    fun downloadLatestUpdate(apkUrl: String) {
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

                if (!isTrustedUpdateUrl(apkUrl)) {
                    runOnUiThread {
                        Toast.makeText(this@MainActivity, "Untrusted update URL rejected", Toast.LENGTH_LONG).show()
                        webView.evaluateJavascript(
                            "if (typeof window.onUpdateDownloadFailed === 'function') window.onUpdateDownloadFailed('Untrusted URL');",
                            null
                        )
                    }
                    return@launch
                }

                var currentUrl = apkUrl
                var connection: java.net.HttpURLConnection? = null
                var redirects = 0
                while (redirects < 8) {
                    if (!isTrustedUpdateUrl(currentUrl)) {
                        connection = null
                        break
                    }
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

                val sharedUri = publishApkToSharedDownloads(destFile)
                val signatureMatches = isApkSignedWithSameKey(destFile)
                runOnUiThread {
                    webView.evaluateJavascript(
                        "if (typeof window.onUpdateDownloadProgress === 'function') window.onUpdateDownloadProgress(100, 'Saved to Downloads/PaheFlow');",
                        null
                    )
                    webView.evaluateJavascript(
                        "if (typeof window.onUpdateDownloadComplete === 'function') window.onUpdateDownloadComplete();",
                        null
                    )
                    showUpdateReadyDialog(sharedUri, signatureMatches)
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

    private fun updateDisplayName(): String {
        val version = try {
            packageManager.getPackageInfo(packageName, 0).versionName ?: "update"
        } catch (e: Exception) {
            "update"
        }
        return "paheflow-update-v$version.apk"
    }

    private fun publishApkToSharedDownloads(tempFile: File): Uri? {
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                val values = ContentValues().apply {
                    put(MediaStore.MediaColumns.DISPLAY_NAME, updateDisplayName())
                    put(MediaStore.MediaColumns.MIME_TYPE, "application/vnd.android.package-archive")
                    put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/PaheFlow")
                }
                val uri = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return null
                val out = contentResolver.openOutputStream(uri) ?: return null
                out.use { output ->
                    tempFile.inputStream().use { input -> input.copyTo(output) }
                }
                uri
            } else {
                @Suppress("DEPRECATION")
                val targetDir = File(
                    Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS),
                    "PaheFlow"
                )
                if (!targetDir.exists()) targetDir.mkdirs()
                val target = File(targetDir, updateDisplayName())
                tempFile.copyTo(target, overwrite = true)
                androidx.core.content.FileProvider.getUriForFile(
                    this,
                    "${packageName}.fileprovider",
                    target
                )
            }
        } catch (e: Exception) {
            null
        }
    }

    private fun showUpdateReadyDialog(apkUri: Uri?, signatureMatches: Boolean) {
        val note = if (signatureMatches) {
            "Open it with your file manager or package installer to install the update."
        } else {
            "Note: this file was signed with a different key than the installed app. If you are upgrading " +
                "from v1.1.0 or older, uninstall the old version first, then install this file with your " +
                "file manager or package installer."
        }
        AlertDialog.Builder(this)
            .setTitle("Update downloaded")
            .setMessage("The update APK was saved to your Downloads/PaheFlow folder.\n\n$note")
            .setPositiveButton("Open with…") { _, _ ->
                if (apkUri == null) {
                    Toast.makeText(this, "Could not share the file - open Downloads/PaheFlow manually", Toast.LENGTH_LONG).show()
                    return@setPositiveButton
                }
                try {
                    val view = Intent(Intent.ACTION_VIEW).apply {
                        setDataAndType(apkUri, "application/vnd.android.package-archive")
                        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                        clipData = android.content.ClipData.newRawUri("update_apk", apkUri)
                        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    }
                    startActivity(Intent.createChooser(view, "Open update with…"))
                } catch (e: Exception) {
                    Toast.makeText(this, "No app can open the APK: ${e.message}", Toast.LENGTH_LONG).show()
                }
            }
            .setNegativeButton("Later", null)
            .show()
    }

    private fun isTrustedUpdateUrl(rawUrl: String): Boolean {
        val uri = Uri.parse(rawUrl)
        if (!"https".equals(uri.scheme, ignoreCase = true)) return false
        val host = uri.host?.lowercase(java.util.Locale.US) ?: return false
        return when (host) {
            "github.com" -> (uri.path ?: "").startsWith("/sumon317/paheflow/")
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
            "github-releases.githubusercontent.com" -> true
            else -> false
        }
    }

    @Suppress("DEPRECATION")
    private fun isApkSignedWithSameKey(apkFile: File): Boolean {
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                val archiveInfo = packageManager.getPackageArchiveInfo(
                    apkFile.absolutePath,
                    PackageManager.GET_SIGNING_CERTIFICATES
                ) ?: return false
                val installedInfo = packageManager.getPackageInfo(
                    packageName,
                    PackageManager.GET_SIGNING_CERTIFICATES
                )
                val archiveCerts = archiveInfo.signingInfo?.apkContentsSigners
                    ?.map { it.toByteArray().sha256Hex() }
                    ?.toSet()
                    ?: return false
                val installedCerts = installedInfo.signingInfo?.apkContentsSigners
                    ?.map { it.toByteArray().sha256Hex() }
                    ?.toSet()
                    ?: return false
                archiveCerts.isNotEmpty() && archiveCerts == installedCerts
            } else {
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
                archiveCerts.isNotEmpty() && archiveCerts == installedCerts
            }
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
        AnimePaheService.performFullShutdown(applicationContext)
    }

    override fun onDestroy() {
        if (currentInstance?.get() == this) {
            currentInstance = null
        }
        webView.destroy()
        super.onDestroy()
    }
}
