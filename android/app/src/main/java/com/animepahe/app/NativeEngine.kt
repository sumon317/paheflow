package com.animepahe.app

object NativeEngine {
    init {
        System.loadLibrary("animepahe_gui")
    }

    external fun startServer(preferredPort: Int, resDir: String, dataDir: String): Int
    external fun getServerPort(): Int
    external fun isServerRunning(): Boolean
    external fun getServerToken(): String
    external fun notifyCloudflareSolved(cfCookie: String, userAgent: String): Boolean
}
