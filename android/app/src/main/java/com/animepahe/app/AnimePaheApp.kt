package com.animepahe.app

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.os.Build
import android.util.Log
import java.io.File
import java.io.FileOutputStream

class AnimePaheApp : Application() {

    companion object {
        const val CHANNEL_ID = "animepahe_downloads"
        const val TAG = "AnimePaheApp"
        lateinit var instance: AnimePaheApp
            private set
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
        extractAssetsIfNeeded()
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

    private fun extractAssetsIfNeeded() {
        try {
            val targetDir = getResourceDir()
            if (!targetDir.exists()) {
                targetDir.mkdirs()
            }
            copyAssetFolder("web", targetDir)
            Log.i(TAG, "Web assets deployed to ${targetDir.absolutePath}")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to extract web assets", e)
        }
    }

    private fun copyAssetFolder(assetPath: String, targetDir: File) {
        val files = assets.list(assetPath) ?: return
        if (files.isEmpty()) {
            // It's a file
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
        if (destFile.exists() && destFile.length() > 0) {
            destFile.delete()
        }
        destFile.parentFile?.mkdirs()
        assets.open(assetPath).use { input ->
            FileOutputStream(destFile).use { output ->
                input.copyTo(output)
            }
        }
    }
}
