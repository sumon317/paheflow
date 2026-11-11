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
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
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
    private var isPolling = false

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
            MainActivity.currentInstance?.get()?.finishAndRemoveTask()
            android.os.Process.killProcess(android.os.Process.myPid())
            return START_NOT_STICKY
        }

        val notification = buildNotification("PaheFlow is active", "Service is ready in background", 0, false)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        startEngineAndMonitor()
        return START_STICKY
    }

    private fun acquireWakeLock() {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = powerManager.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "PaheFlow::DownloadWakeLock"
        ).apply {
            setReferenceCounted(false)
            acquire(10 * 60 * 1000L /* 10 minutes max fallback */)
        }
    }

    private fun startEngineAndMonitor() {
        serviceScope.launch {
            val app = AnimePaheApp.instance
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
                    val job = json.optJSONObject("job")
                    if (job != null && job.optBoolean("running", false)) {
                        val percent = job.optDouble("percent", 0.0).toInt()
                        val speed = job.optString("speed", "")
                        val ep = job.optString("current_episode", "")
                        val text = if (ep.isNotEmpty()) "Ep $ep • $speed" else "Downloading… $speed"
                        updateNotification("Downloading anime", text, percent, true)
                        wakeLock?.acquire(5 * 60 * 1000L)
                    } else {
                        updateNotification("PaheFlow", "Idle • Tap to open", 0, false)
                    }
                }
                conn.disconnect()
            } catch (e: Exception) {
                // Ignore transient local connection blips
            }
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
