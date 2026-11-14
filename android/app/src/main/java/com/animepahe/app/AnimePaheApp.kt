package com.animepahe.app

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.content.pm.PackageInfoCompat
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import java.io.File
import java.io.FileOutputStream

class AnimePaheApp : Application() {

    companion object {
        const val CHANNEL_ID = "animepahe_downloads"
        const val TAG = "AnimePaheApp"
        private const val STAMP_FILE = ".assets_stamp"
        lateinit var instance: AnimePaheApp
            private set
    }

    private val appScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private val assetsReady = CompletableDeferred<Unit>()

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()

        val targetDir = getResourceDir()
        val stamp = currentAssetStamp()
        val valid = areAssetsValid(targetDir, stamp)
        if (valid) {
            assetsReady.complete(Unit)
        }

        // Run directory init, native .so preload, and (if needed) asset extraction off the main thread.
        appScope.launch {
            initStorageDirectories()
            // Preload libanimepahe_gui.so in parallel with Activity/WebView setup
            NativeEngine.isServerRunning()
            if (!valid) {
                extractAssets(targetDir, stamp)
                assetsReady.complete(Unit)
            }
        }
    }

    suspend fun awaitAssetsReady() {
        assetsReady.await()
    }

    private fun initStorageDirectories() {
        try {
            getExternalFilesDir(android.os.Environment.DIRECTORY_DOWNLOADS)?.mkdirs()
            (externalCacheDir ?: cacheDir).mkdirs()
        } catch (e: Exception) {
            Log.w(TAG, "Could not initialize app storage directories: ${e.message}")
        }
    }

    fun getExternalDownloadDir(): File {
        return getExternalFilesDir(android.os.Environment.DIRECTORY_DOWNLOADS)
            ?: File(filesDir, "downloads")
    }

    fun getAppCacheDir(): File {
        return (externalCacheDir ?: cacheDir).apply { mkdirs() }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val name = "PaheFlow Downloads"
            val descriptionText = "Live download progress and background task notifications"
            val importance = NotificationManager.IMPORTANCE_LOW
            val channel = NotificationChannel(CHANNEL_ID, name, importance).apply {
                description = descriptionText
                setShowBadge(false)
            }
            val notificationManager: NotificationManager =
                getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }
    }

    fun getResourceDir(): File {
        return File(filesDir, "web")
    }

    fun getAppDataDir(): File {
        return File(filesDir, "data").apply { mkdirs() }
    }

    @Suppress("DEPRECATION")
    private fun currentAssetStamp(): String {
        return try {
            val pkg = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                packageManager.getPackageInfo(packageName, PackageManager.PackageInfoFlags.of(0))
            } else {
                packageManager.getPackageInfo(packageName, 0)
            }
            val versionCode = PackageInfoCompat.getLongVersionCode(pkg)
            "${versionCode}_${pkg.lastUpdateTime}"
        } catch (_: Exception) {
            "unknown"
        }
    }

    private fun areAssetsValid(targetDir: File, expectedStamp: String): Boolean {
        if (expectedStamp == "unknown") return false
        val stampFile = File(targetDir, STAMP_FILE)
        if (!stampFile.isFile) return false
        val savedStamp = try {
            stampFile.readText().trim()
        } catch (_: Exception) {
            return false
        }
        if (savedStamp != expectedStamp) return false

        return File(targetDir, "static/index.html").let { it.isFile && it.length() > 0 } &&
            File(targetDir, "static/app.js").let { it.isFile && it.length() > 0 } &&
            File(targetDir, "static/styles.css").let { it.isFile && it.length() > 0 } &&
            File(targetDir, "anime.list").let { it.isFile && it.length() > 0 }
    }

    private fun extractAssets(targetDir: File, stamp: String) {
        try {
            if (!targetDir.exists()) {
                targetDir.mkdirs()
            }
            copyAssetFolder("web", targetDir)
            if (stamp != "unknown") {
                File(targetDir, STAMP_FILE).writeText(stamp)
            }
            Log.i(TAG, "Web assets deployed to ${targetDir.absolutePath}")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to extract web assets", e)
        }
    }

    private fun copyAssetFolder(assetPath: String, targetDir: File) {
        val files = assets.list(assetPath) ?: return
        if (files.isEmpty()) {
            copyAssetFile(assetPath, File(targetDir, File(assetPath).name))
        } else {
            for (filename in files) {
                val subAsset = if (assetPath.isEmpty()) filename else "$assetPath/$filename"
                val subFiles = assets.list(subAsset)
                if (subFiles != null && subFiles.isNotEmpty()) {
                    val subDir = File(targetDir, filename)
                    subDir.mkdirs()
                    copyAssetFolder(subAsset, subDir)
                } else {
                    copyAssetFile(subAsset, File(targetDir, filename))
                }
            }
        }
    }

    private fun copyAssetFile(assetPath: String, destFile: File) {
        destFile.parentFile?.mkdirs()
        assets.open(assetPath).use { input ->
            FileOutputStream(destFile).use { output ->
                input.copyTo(output, bufferSize = 16384)
            }
        }
    }
}
