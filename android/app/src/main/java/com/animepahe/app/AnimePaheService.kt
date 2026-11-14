package com.animepahe.app

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
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
import java.lang.ref.WeakReference
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.system.exitProcess

class AnimePaheService : Service() {

    companion object {
        const val TAG = "AnimePaheService"
        const val NOTIFICATION_ID = 1001
        const val ACTION_START = "com.animepahe.app.action.START"
        const val ACTION_STOP = "com.animepahe.app.action.STOP"

        @Volatile
        private var currentInstance: WeakReference<AnimePaheService>? = null

        @Volatile
        var hasActiveDownload: Boolean = false
            private set

        private val isShuttingDown = AtomicBoolean(false)

        fun start(context: Context) {
            if (isShuttingDown.get()) return
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
            performFullShutdown(context.applicationContext)
        }

        fun stopIfIdle(context: Context) {
            if (hasActiveDownload || isShuttingDown.get()) return
            val svc = currentInstance?.get()
            if (svc != null) {
                svc.stopIdleSelf()
            } else {
                val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
                nm?.cancel(NOTIFICATION_ID)
                context.stopService(Intent(context, AnimePaheService::class.java))
            }
        }

        fun performFullShutdown(context: Context, onFinished: (() -> Unit)? = null) {
            if (!isShuttingDown.compareAndSet(false, true)) {
                onFinished?.invoke()
                return
            }

            hasActiveDownload = false
            val appCtx = context.applicationContext
            val nm = appCtx.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
            nm?.cancel(NOTIFICATION_ID)

            val mainHandler = Handler(Looper.getMainLooper())
            mainHandler.post {
                currentInstance?.get()?.stopForShutdown()
                MainActivity.currentInstance?.get()?.let { activity ->
                    if (!activity.isFinishing && !activity.isDestroyed) {
                        activity.finishAndRemoveTask()
                    }
                }
            }

            Thread({
                try {
                    NativeEngine.shutdownEngine()
                } catch (e: Throwable) {
                    Log.w(TAG, "Error stopping native engine: ${e.message}")
                }
                // Let stopSelf()/onDestroy() settle with ActivityManagerService before exiting
                // so the OS does not treat shutdown as an unexpected process crash.
                try {
                    Thread.sleep(300)
                } catch (_: InterruptedException) {}

                nm?.cancel(NOTIFICATION_ID)
                onFinished?.invoke()
                try {
                    Thread.sleep(50)
                } catch (_: InterruptedException) {}
                exitProcess(0)
            }, "PaheFlow-Shutdown").start()
        }
    }

    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private val isPolling = AtomicBoolean(false)
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var isForegroundStarted = false
    private var lastCompletedCount = 0
    private val scannedFiles = HashSet<String>()

    private var lastNotifKey: String? = null
    private var lastNotifTitle = "PaheFlow"
    private var lastNotifText = "Ready in background"
    private var lastNotifProgress = 0
    private var lastNotifIndeterminate = false

    override fun onCreate() {
        super.onCreate()
        currentInstance = WeakReference(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP || isShuttingDown.get()) {
            performFullShutdown(applicationContext)
            return START_NOT_STICKY
        }

        if (!isForegroundStarted) {
            val notification = buildNotification(
                lastNotifTitle,
                lastNotifText,
                lastNotifProgress,
                lastNotifIndeterminate
            )
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
                isForegroundStarted = true
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start foreground service: ${e.message}", e)
            }
        }

        startEngineAndMonitor()
        return START_NOT_STICKY
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        super.onTaskRemoved(rootIntent)
        if (!hasActiveDownload && !isShuttingDown.get()) {
            Log.i(TAG, "App swiped from recents while idle; stopping background service")
            stopIdleSelf()
        }
    }

    private fun stopIdleSelf() {
        releaseLocks()
        stopForegroundCompat()
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        nm?.cancel(NOTIFICATION_ID)
        stopSelf()
    }

    private fun stopForShutdown() {
        releaseLocks()
        stopForegroundCompat()
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        nm?.cancel(NOTIFICATION_ID)
        stopSelf()
    }

    private fun stopForegroundCompat() {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(STOP_FOREGROUND_REMOVE)
            } else {
                @Suppress("DEPRECATION")
                stopForeground(true)
            }
        } catch (_: Exception) {}
        isForegroundStarted = false
    }

    private fun acquireWakeLock() {
        try {
            if (wakeLock == null) {
                val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
                wakeLock = powerManager.newWakeLock(
                    PowerManager.PARTIAL_WAKE_LOCK,
                    "PaheFlow::DownloadWakeLock"
                ).apply {
                    setReferenceCounted(false)
                }
            }
            if (wakeLock?.isHeld != true) {
                wakeLock?.acquire(6 * 60 * 60 * 1000L)
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not acquire WakeLock: ${e.message}")
        }
    }

    private fun releaseWakeLock() {
        try {
            wakeLock?.let { if (it.isHeld) it.release() }
        } catch (_: Exception) {}
    }

    private fun acquireWifiLock() {
        if (wifiLock?.isHeld == true) return
        try {
            val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
            val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            } else {
                @Suppress("DEPRECATION")
                WifiManager.WIFI_MODE_FULL_HIGH_PERF
            }
            wifiLock = wifiManager?.createWifiLock(mode, "PaheFlow::DownloadWifiLock")?.apply {
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

    private fun releaseLocks() {
        releaseWakeLock()
        releaseWifiLock()
    }

    private fun startEngineAndMonitor() {
        if (!isPolling.compareAndSet(false, true)) return
        serviceScope.launch {
            val app = AnimePaheApp.instance
            app.awaitAssetsReady()
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

            if (port > 0 && !isShuttingDown.get()) {
                pollJobProgress(port)
            } else {
                isPolling.set(false)
            }
        }
    }

    private fun JSONObject?.optCleanString(key: String, fallback: String = ""): String {
        if (this == null || !has(key) || isNull(key)) return fallback
        val value = optString(key, fallback).trim()
        return if (value.equals("null", ignoreCase = true)) fallback else value
    }

    private suspend fun pollJobProgress(port: Int) {
        var token = NativeEngine.getServerToken()
        while (!isShuttingDown.get()) {
            var nextDelayMs = 3500L
            var conn: HttpURLConnection? = null
            try {
                if (token.isEmpty()) {
                    token = NativeEngine.getServerToken()
                }
                val url = URL("http://127.0.0.1:$port/api/status")
                conn = (url.openConnection() as HttpURLConnection).apply {
                    connectTimeout = 1500
                    readTimeout = 1500
                    setRequestProperty("x-app-token", token)
                }

                if (conn.responseCode == 200) {
                    val body = conn.inputStream.bufferedReader().use { it.readText() }
                    val json = JSONObject(body)
                    val job = json.optJSONObject("job") ?: json
                    val isRunning = job.optBoolean("running", false)
                    val isPaused = job.optBoolean("paused", false)
                    val queueHold = job.optBoolean("queueHold", false)
                    val queueArr = job.optJSONArray("queue")

                    var queuedCount = 0
                    if (queueArr != null) {
                        for (i in 0 until queueArr.length()) {
                            val entry = queueArr.optJSONObject(i) ?: continue
                            val st = entry.optCleanString("status")
                            if (st == "queued" || st == "downloading") {
                                queuedCount++
                            }
                        }
                    }

                    hasActiveDownload = isRunning || (queuedCount > 0 && !queueHold)

                    if (isRunning) {
                        val progress = job.optJSONObject("progress")
                        val rawPercent = if (progress != null && !progress.isNull("percent")) {
                            progress.optDouble("percent", 0.0).toInt().coerceIn(0, 100)
                        } else {
                            0
                        }
                        val speed = progress.optCleanString("speed")
                        val epObj = job.optJSONObject("episodes")
                        val ep = epObj.optCleanString("current")
                        val animeObj = job.optJSONObject("anime")
                        val animeTitle = animeObj.optCleanString("title", "anime")

                        if (isPaused) {
                            releaseLocks()
                            val subParts = listOfNotNull(
                                if (ep.isNotEmpty()) "Ep $ep" else null,
                                "Paused"
                            )
                            updateNotification(
                                "Paused • $animeTitle",
                                subParts.joinToString(" • "),
                                rawPercent,
                                false
                            )
                            nextDelayMs = 2500L
                        } else {
                            acquireWakeLock()
                            acquireWifiLock()
                            val remainingQueued = (queuedCount - 1).coerceAtLeast(0)
                            val subParts = listOfNotNull(
                                if (ep.isNotEmpty()) "Ep $ep" else null,
                                if (speed.isNotEmpty()) speed else null,
                                if (remainingQueued > 0) "$remainingQueued queued" else null
                            )
                            val text = if (subParts.isNotEmpty()) {
                                subParts.joinToString(" • ")
                            } else {
                                "Preparing download…"
                            }
                            updateNotification("Downloading $animeTitle", text, rawPercent, true)
                            nextDelayMs = 1500L
                        }

                        val completedArr = epObj?.optJSONArray("completed")
                        val completedCount = completedArr?.length() ?: 0
                        if (completedCount > lastCompletedCount) {
                            lastCompletedCount = completedCount
                            scanNewCompletedFiles()
                        }
                    } else {
                        releaseLocks()
                        if (lastCompletedCount > 0) {
                            scanNewCompletedFiles()
                            lastCompletedCount = 0
                        }
                        if (queuedCount > 0) {
                            val statusText = if (queueHold) {
                                "Queue paused • Tap to open"
                            } else {
                                "Starting next episode…"
                            }
                            updateNotification("PaheFlow • $queuedCount queued", statusText, 0, false)
                            nextDelayMs = if (queueHold) 3500L else 1500L
                        } else {
                            updateNotification("PaheFlow", "Idle • Tap to open", 0, false)
                            nextDelayMs = 3500L
                        }
                    }
                }
            } catch (_: Exception) {
                // Ignore transient local connection blips
            } finally {
                try {
                    conn?.disconnect()
                } catch (_: Exception) {}
            }
            delay(nextDelayMs)
        }
        isPolling.set(false)
    }

    private fun scanNewCompletedFiles() {
        try {
            val dirs = mutableListOf<java.io.File>()
            val pubDir = java.io.File(
                android.os.Environment.getExternalStoragePublicDirectory(
                    android.os.Environment.DIRECTORY_DOWNLOADS
                ),
                "PaheFlow"
            )
            if (pubDir.exists()) dirs.add(pubDir)
            val extDir = AnimePaheApp.instance.getExternalDownloadDir()
            if (extDir.exists()) dirs.add(extDir)

            val newlyFound = mutableListOf<String>()
            dirs.forEach { base ->
                base.walkTopDown().maxDepth(3).filter {
                    it.isFile && (it.extension.equals("mp4", ignoreCase = true) ||
                        it.extension.equals("mkv", ignoreCase = true))
                }.forEach { videoFile ->
                    val path = videoFile.absolutePath
                    if (scannedFiles.add(path)) {
                        newlyFound.add(path)
                    }
                }
            }

            if (newlyFound.isNotEmpty()) {
                android.media.MediaScannerConnection.scanFile(
                    this@AnimePaheService,
                    newlyFound.toTypedArray(),
                    Array(newlyFound.size) { "video/mp4" },
                    null
                )
            }
        } catch (e: Exception) {
            Log.w(TAG, "Error scanning completed media files: ${e.message}")
        }
    }

    private fun buildNotification(
        title: String,
        text: String,
        progress: Int,
        isIndeterminate: Boolean
    ): Notification {
        val launchIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val immutableFlag = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_IMMUTABLE
        } else {
            0
        }
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            launchIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or immutableFlag
        )

        val builder = NotificationCompat.Builder(this, AnimePaheApp.CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)

        val stopIntent = Intent(this, ShutdownReceiver::class.java).apply {
            action = ACTION_STOP
        }
        val stopPendingIntent = PendingIntent.getBroadcast(
            this,
            1,
            stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or immutableFlag
        )
        builder.addAction(
            android.R.drawable.ic_menu_close_clear_cancel,
            "Shut down",
            stopPendingIntent
        )

        if (progress > 0 || isIndeterminate) {
            builder.setProgress(100, progress, isIndeterminate && progress == 0)
        }

        return builder.build()
    }

    private fun updateNotification(
        title: String,
        text: String,
        progress: Int,
        isIndeterminate: Boolean
    ) {
        if (isShuttingDown.get() || !isForegroundStarted) return
        val key = "$title|$text|$progress|$isIndeterminate"
        if (key == lastNotifKey) return

        lastNotifKey = key
        lastNotifTitle = title
        lastNotifText = text
        lastNotifProgress = progress
        lastNotifIndeterminate = isIndeterminate

        val notification = buildNotification(title, text, progress, isIndeterminate)
        val notificationManager =
            getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        notificationManager.notify(NOTIFICATION_ID, notification)
    }

    override fun onDestroy() {
        if (currentInstance?.get() == this) {
            currentInstance = null
        }
        isForegroundStarted = false
        isPolling.set(false)
        serviceScope.cancel()
        releaseLocks()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
