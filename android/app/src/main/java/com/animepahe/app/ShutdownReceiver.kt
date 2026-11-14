package com.animepahe.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

class ShutdownReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        if (intent?.action == AnimePaheService.ACTION_STOP) {
            val pendingResult = goAsync()
            AnimePaheService.performFullShutdown(context.applicationContext) {
                pendingResult.finish()
            }
        }
    }
}
