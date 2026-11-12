package com.animepahe.app

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

class AnimePaheService : Service() {

    companion object {
        const val TAG = "AnimePaheService"
        const val NOTIFICATION_ID = 1001
        const val ACTION_START = "com.animepahe.app.action.START"
        const val ACTION_STOP = "com.animepahe.app.action.STOP"

        fun start(context: Context) {
            val intent = Intent(context, AnimePaheService::class.java).apply {
                action = ACTION_START
            }
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(intent)
                } else {
                    context.startService(intent)
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start AnimePaheService: ${e.message}", e)
            }
        }

        fun stop(context: Context) {
            val intent = Intent(context, AnimePaheService::class.java).apply {
                action = ACTION_STOP
            }
            context.stopService(intent)
        }
    }

    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: android.net.wifi.WifiManager.WifiLock? = null
    private var isPolling = false
    private var lastCompletedCount = 0

    override fun onCreate() {
        super.onCreate()
        acquireWakeLock()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(STOP_FOREGROUND_REMOVE)
            } else {
                @Suppress("DEPRECATION")
                stopForeground(true)
            }
            stopSelf()
            wakeLock?.let { if (it.isHeld) it.release() }
            releaseWifiLock()
            MainActivity.currentInstance?.get()?.finishAndRemoveTask()
            android.os.Process.killProcess(android.os.Process.myPid())
            return START_NOT_STICKY
        }

        val notification = buildNotification("PaheFlow is active", "Service is ready in background", 0, false)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                )
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start foreground service: ${e.message}", e)
        }

        startEngineAndMonitor()
        return START_STICKY
    }

    private fun acquireWakeLock() {
        try {
            val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
            if (wakeLock == null) {
                wakeLock = powerManager.newWakeLock(
                    PowerManager.PARTIAL_WAKE_LOCK,
                    "PaheFlow::DownloadWakeLock"
                ).apply {
                    setReferenceCounted(false)
                }
            }
            if (wakeLock?.isHeld != true) {
                wakeLock?.acquire()
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not acquire WakeLock: ${e.message}")
        }
    }

    private fun acquireWifiLock() {
        if (wifiLock?.isHeld == true) return
        try {
            val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as? android.net.wifi.WifiManager
            wifiLock = wifiManager?.createWifiLock(
                android.net.wifi.WifiManager.WIFI_MODE_FULL_HIGH_PERF,
                "PaheFlow::DownloadWifiLock"
            )?.apply {
                setReferenceCounted(false)
                acquire()
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not acquire WifiLock: ${e.message}")
        }
    }

    private fun releaseWifiLock() {
        try {
            wifiLock?.let { if (it.isHeld) it.release() }
        } catch (_: Exception) {}
    }

    private fun startEngineAndMonitor() {
        serviceScope.launch {
            val app = AnimePaheApp.instance
            try {
                android.system.Os.setenv("ANIMEPAHE_APP_EXT_DOWNLOAD_DIR", app.getExternalDownloadDir().absolutePath, true)
                android.system.Os.setenv("ANIMEPAHE_GUI_CACHE_DIR", app.getAppCacheDir().absolutePath, true)
            } catch (e: Exception) {
                Log.w(TAG, "Could not set native storage environment variables: ${e.message}")
            }

            val port = NativeEngine.startServer(
                preferredPort = 8765,
                resDir = app.getResourceDir().absolutePath,
                dataDir = app.getAppDataDir().absolutePath
            )
            Log.i(TAG, "Native engine started on port $port")

            if (port > 0 && !isPolling) {
                isPolling = true
                pollJobProgress(port)
            }
        }
    }

    private suspend fun pollJobProgress(port: Int) {
        val token = NativeEngine.getServerToken()
        while (true) {
            delay(1500)
            try {
                val url = URL("http://127.0.0.1:$port/api/status")
                val conn = url.openConnection() as HttpURLConnection
                conn.connectTimeout = 1500
                conn.readTimeout = 1500
                conn.setRequestProperty("x-app-token", token)

                if (conn.responseCode == 200) {
                    val body = conn.inputStream.bufferedReader().readText()
                    val json = JSONObject(body)
                    val job = json.optJSONObject("job") ?: json
                    val isRunning = job.optBoolean("running", false)
                    if (isRunning) {
                        val progress = job.optJSONObject("progress")
                        val percent = progress?.optDouble("percent", 0.0)?.toInt() ?: 0
                        val speed = progress?.optString("speed", "") ?: ""
                        val epObj = job.optJSONObject("episodes")
                        val ep = epObj?.optString("current", "") ?: ""
                        val animeObj = job.optJSONObject("anime")
                        val animeTitle = animeObj?.optString("title", "") ?: "anime"
                        val text = if (ep.isNotEmpty()) "Ep $ep • $speed" else "Downloading $animeTitle… $speed"
                        updateNotification("Downloading $animeTitle", text, percent, true)

                        if (wakeLock?.isHeld != true) {
                            acquireWakeLock()
                        }
                        acquireWifiLock()

                        val completedArr = epObj?.optJSONArray("completed")
                        val completedCount = completedArr?.length() ?: 0
                        if (completedCount > lastCompletedCount) {
                            lastCompletedCount = completedCount
                            scanAllCompletedFiles()
                        }
                    } else {
                        releaseWifiLock()
                        updateNotification("PaheFlow", "Idle • Tap to open", 0, false)
                        if (lastCompletedCount > 0) {
                            scanAllCompletedFiles()
                            lastCompletedCount = 0
                        }
                    }
                }
                conn.disconnect()
            } catch (e: Exception) {
                // Ignore transient local connection blips
            }
        }
    }

    private fun scanAllCompletedFiles() {
        try {
            val dirs = mutableListOf<java.io.File>()
            val pubDir = java.io.File(android.os.Environment.getExternalStoragePublicDirectory(android.os.Environment.DIRECTORY_DOWNLOADS), "PaheFlow")
            if (pubDir.exists()) dirs.add(pubDir)
            val extDir = AnimePaheApp.instance.getExternalDownloadDir()
            if (extDir.exists()) dirs.add(extDir)

            dirs.forEach { base ->
                base.walkTopDown().maxDepth(3).filter {
                    it.isFile && (it.extension.equals("mp4", ignoreCase = true) || it.extension.equals("mkv", ignoreCase = true))
                }.forEach { videoFile ->
                    android.media.MediaScannerConnection.scanFile(
                        this@AnimePaheService,
                        arrayOf(videoFile.absolutePath),
                        arrayOf("video/mp4"),
                        null
                    )
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Error scanning completed media files: ${e.message}")
        }
    }

    private fun buildNotification(title: String, text: String, progress: Int, isIndeterminate: Boolean): Notification {
        val launchIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            launchIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
        )

        val builder = NotificationCompat.Builder(this, AnimePaheApp.CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)

        val stopIntent = Intent(this, AnimePaheService::class.java).apply {
            action = ACTION_STOP
        }
        val stopPendingIntent = PendingIntent.getService(
            this,
            1,
            stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
        )
        builder.addAction(android.R.drawable.ic_menu_close_clear_cancel, "Shut down", stopPendingIntent)

        if (progress > 0 || isIndeterminate) {
            builder.setProgress(100, progress, isIndeterminate && progress == 0)
        }

        return builder.build()
    }

    private fun updateNotification(title: String, text: String, progress: Int, isIndeterminate: Boolean) {
        val notification = buildNotification(title, text, progress, isIndeterminate)
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as android.app.NotificationManager
        notificationManager.notify(NOTIFICATION_ID, notification)
    }

    override fun onDestroy() {
        serviceScope.cancel()
        wakeLock?.let {
            if (it.isHeld) it.release()
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
