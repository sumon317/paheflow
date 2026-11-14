package com.animepahe.app

import android.annotation.SuppressLint
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock

/**
 * Hidden Chromium WebView HTTP bridge for AnimePahe requests on Android.
 *
 * Cloudflare binds `cf_clearance` to Chromium's TLS JA4 fingerprint, HTTP/2
 * frames, `Sec-CH-UA-*` Client Hints, and full CookieManager cookie jar
 * (`__cf_bm`, session cookies, etc.). Rust's `reqwest` (`rustls-tls`) gets
 * HTTP 403 challenged by Cloudflare on `/play/...` and `/api?...` even with a
 * fresh `cf_clearance`. Executing requests through this shared Chromium WebView
 * keeps the exact TLS fingerprint, cookies, and Client Hints that solved
 * Cloudflare in [CloudflareBottomSheet].
 */
object AndroidHttpBridge {

    private const val TAG = "AndroidHttpBridge"
    private const val FETCH_TIMEOUT_SECS = 28L
    private const val NAV_TIMEOUT_MS = 22_000L
    private const val CF_WAIT_MAX_MS = 6_500L
    private const val MAX_WEBVIEWS = 4

    private val mainHandler = Handler(Looper.getMainLooper())
    private val fetchLock = ReentrantLock()

    private val webViewsByOrigin = LinkedHashMap<String, WebView>(4, 0.75f, true)
    private val loadedOrigins = HashSet<String>()

    @Volatile
    private var pendingChallengeOrigin: String? = null

    private data class FetchResult(
        val ok: Boolean,
        val status: Int,
        val body: String,
        val error: String,
        val cfChallenge: Boolean,
    )

    private class PendingFetch(val reqId: String) {
        val latch = CountDownLatch(1)

        @Volatile
        var status: Int = 0

        @Volatile
        var body: String = ""

        @Volatile
        var error: String = ""
    }

    private class PendingNav(val navId: String) {
        val latch = CountDownLatch(1)

        @Volatile
        var httpStatus: Int = 200

        @Volatile
        var pageGeneration: Long = 0

        @Volatile
        var netError: String? = null

        @Volatile
        var html: String = ""

        @Volatile
        var cfChallenge: Boolean = false

        @Volatile
        var settled: Boolean = false
    }

    @Volatile
    private var activeFetch: PendingFetch? = null

    @Volatile
    private var activeNav: PendingNav? = null

    @JvmStatic
    fun onCredentialsUpdated() {
        loadedOrigins.clear()
    }

    @JvmStatic
    fun getPendingChallengeOrigin(): String? = pendingChallengeOrigin

    @JvmStatic
    fun clearPendingChallengeOrigin() {
        pendingChallengeOrigin = null
    }

    @JvmStatic
    fun fetch(url: String, referer: String, cfCookie: String, userAgent: String): String {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            return encodeResponse(
                ok = false,
                status = 0,
                body = "",
                error = "AndroidHttpBridge.fetch must not be called on the main thread",
                cfChallenge = false
            )
        }

        fetchLock.lock()
        try {
            val result = executeLocked(url, referer, cfCookie, userAgent)
            return encodeResponse(
                ok = result.ok,
                status = result.status,
                body = result.body,
                error = result.error,
                cfChallenge = result.cfChallenge
            )
        } catch (t: Throwable) {
            Log.e(TAG, "Unexpected WebView bridge error for $url", t)
            return encodeResponse(
                ok = false,
                status = 0,
                body = "",
                error = "WebView bridge error: ${t.message ?: t.javaClass.simpleName}",
                cfChallenge = false
            )
        } finally {
            fetchLock.unlock()
        }
    }

    private fun executeLocked(
        url: String,
        referer: String,
        cfCookie: String,
        userAgent: String
    ): FetchResult {
        val origin = extractOrigin(url)
            ?: return FetchResult(false, 0, "", "Invalid URL for WebView bridge: $url", false)

        if (cfCookie.isNotBlank()) {
            syncManualCookieIfNeeded(origin, cfCookie)
        }

        val wv = ensureWebViewOnMain(origin, userAgent)
            ?: return FetchResult(false, 0, "", "Failed to initialize Android WebView", false)

        val isApiCall = url.contains("/api")
        val refOrigin = if (referer.isNotBlank()) extractOrigin(referer) else null
        val isCrossOriginDoc = !isApiCall && refOrigin != null && refOrigin != origin

        // Cross-origin document fetches (e.g., https://kwik.cx/e/... with Referer: https://animepahe.pw/)
        // need a direct top-level navigation so Chromium sends the exact cross-origin Referer and
        // navigation Sec-Fetch headers and lets Cloudflare's JS challenge run if needed.
        if (isCrossOriginDoc) {
            val navResult = navigateAndRead(wv, url, referer)
            if (navResult.ok) {
                loadedOrigins.add(origin)
                if (pendingChallengeOrigin == origin) {
                    pendingChallengeOrigin = null
                }
                return navResult
            }
            if (navResult.cfChallenge || navResult.status == 403) {
                loadedOrigins.remove(origin)
                pendingChallengeOrigin = origin
            }
            return navResult
        }

        val wasLoaded = origin in loadedOrigins
        if (!wasLoaded) {
            val originNav = navigateAndRead(wv, "$origin/", null)
            if (!originNav.ok) {
                loadedOrigins.remove(origin)
                if (originNav.cfChallenge || originNav.status == 403) {
                    pendingChallengeOrigin = origin
                }
                return originNav
            }
            loadedOrigins.add(origin)
        }

        var apiResult = runJsFetch(wv, url, referer, origin)
        if (apiResult.ok) {
            if (pendingChallengeOrigin == origin) {
                pendingChallengeOrigin = null
            }
            return apiResult
        }

        // If the cached WebView page session went stale (e.g. Cloudflare rotated __cf_bm or a silent
        // JS token), refresh the origin page once so Cloudflare passes silently before failing.
        if (wasLoaded && (apiResult.status == 403 || apiResult.cfChallenge || apiResult.status == 0)) {
            Log.d(TAG, "JS fetch got ${apiResult.status} on cached $origin; refreshing origin page")
            val refreshNav = navigateAndRead(wv, "$origin/", null)
            if (refreshNav.ok) {
                loadedOrigins.add(origin)
                apiResult = runJsFetch(wv, url, referer, origin)
                if (apiResult.ok) {
                    if (pendingChallengeOrigin == origin) {
                        pendingChallengeOrigin = null
                    }
                    return apiResult
                }
            } else {
                loadedOrigins.remove(origin)
                if (refreshNav.cfChallenge || refreshNav.status == 403) {
                    pendingChallengeOrigin = origin
                }
                return refreshNav
            }
        }

        // Non-API pages (e.g., /play/{slug}/{session}) can require top-level
        // navigation headers (`sec-fetch-mode: navigate`, `sec-fetch-dest: document`).
        if (!isApiCall && (apiResult.status == 403 || apiResult.cfChallenge || apiResult.status == 0)) {
            Log.d(TAG, "JS fetch got ${apiResult.status} for $url; trying document navigation")
            val navResult = navigateAndRead(wv, url, referer.ifBlank { "$origin/" })
            if (navResult.ok) {
                loadedOrigins.add(origin)
                if (pendingChallengeOrigin == origin) {
                    pendingChallengeOrigin = null
                }
                return navResult
            }
            if (navResult.cfChallenge || navResult.status == 403) {
                loadedOrigins.remove(origin)
                pendingChallengeOrigin = origin
            }
            return navResult
        }

        if (apiResult.cfChallenge || apiResult.status == 403) {
            loadedOrigins.remove(origin)
            pendingChallengeOrigin = origin
        }
        return apiResult
    }

    private fun runJsFetch(
        wv: WebView,
        url: String,
        referer: String,
        origin: String
    ): FetchResult {
        val reqId = UUID.randomUUID().toString()
        val pending = PendingFetch(reqId)
        activeFetch = pending

        val isApi = url.contains("/api")
        val quotedReqId = JSONObject.quote(reqId)
        val quotedUrl = JSONObject.quote(url)
        val sameOriginRef = if (referer.isNotBlank() && extractOrigin(referer) == origin) {
            JSONObject.quote(referer)
        } else {
            "null"
        }
        val acceptHeader = if (isApi) {
            "application/json, text/javascript, */*; q=0.01"
        } else {
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        }
        val xrwLine = if (isApi) """"X-Requested-With": "XMLHttpRequest",""" else ""

        val script = """
            (function() {
                var reqId = $quotedReqId;
                var targetUrl = $quotedUrl;
                var ref = $sameOriginRef;
                var ctrl = new AbortController();
                var timer = setTimeout(function() { ctrl.abort(); }, 23000);
                var opts = {
                    method: "GET",
                    credentials: "include",
                    signal: ctrl.signal,
                    headers: {
                        $xrwLine
                        "Accept": ${JSONObject.quote(acceptHeader)}
                    }
                };
                if (ref) {
                    opts.referrer = ref;
                }
                fetch(targetUrl, opts).then(function(resp) {
                    return resp.text().then(function(txt) {
                        clearTimeout(timer);
                        window.__paheBridge.onFetchResult(reqId, resp.status, txt || "", "");
                    });
                }).catch(function(err) {
                    clearTimeout(timer);
                    var msg = (err && err.message) ? String(err.message) : String(err || "fetch failed");
                    window.__paheBridge.onFetchResult(reqId, 0, "", msg);
                });
            })();
        """.trimIndent()

        mainHandler.post {
            wv.evaluateJavascript(script, null)
        }

        val completed = pending.latch.await(FETCH_TIMEOUT_SECS, TimeUnit.SECONDS)
        if (activeFetch === pending) {
            activeFetch = null
        }

        if (!completed) {
            return FetchResult(false, 0, "", "WebView fetch timed out for $url", false)
        }

        if (pending.error.isNotEmpty() && pending.status == 0) {
            return FetchResult(false, 0, "", "WebView fetch failed: ${pending.error}", false)
        }

        return classifyHttpResult(pending.status, pending.body)
    }

    private fun navigateAndRead(wv: WebView, url: String, referer: String?): FetchResult {
        val navId = UUID.randomUUID().toString()
        val pending = PendingNav(navId)
        activeNav = pending

        mainHandler.post {
            if (activeNav !== pending) return@post
            if (!referer.isNullOrBlank()) {
                wv.loadUrl(url, mapOf("Referer" to referer))
            } else {
                wv.loadUrl(url)
            }
        }

        val completed = pending.latch.await(NAV_TIMEOUT_MS, TimeUnit.MILLISECONDS)
        if (activeNav === pending) {
            activeNav = null
        }

        if (!completed) {
            return FetchResult(false, 0, "", "WebView navigation timed out for $url", false)
        }

        pending.netError?.let { netErr ->
            return FetchResult(false, 0, "", "WebView network error: $netErr", false)
        }

        if (pending.cfChallenge || pending.httpStatus == 403) {
            return FetchResult(
                ok = false,
                status = 403,
                body = pending.html,
                error = "Cloudflare is challenging this request (HTTP 403 challenge). Tap 'Solve Cloudflare' or update cf_clearance in Settings.",
                cfChallenge = true
            )
        }

        return classifyHttpResult(pending.httpStatus, pending.html)
    }

    private fun classifyHttpResult(status: Int, body: String): FetchResult {
        val lower = body.lowercase()
        val hasChallengeMarkers = isChallengeBody(lower)
        val isBlocked = lower.contains("sorry, you have been blocked") ||
            lower.contains("cf-error-details")

        if (isBlocked) {
            return FetchResult(
                ok = false,
                status = if (status > 0) status else 403,
                body = body,
                error = "Cloudflare refused this request (IP or cookie blocked). Tap 'Solve Cloudflare' or copy a fresh cf_clearance into Settings.",
                cfChallenge = true
            )
        }

        if (status == 403 || hasChallengeMarkers) {
            return FetchResult(
                ok = false,
                status = if (status > 0) status else 403,
                body = body,
                error = "Cloudflare is challenging this request (HTTP 403 challenge). Tap 'Solve Cloudflare' or update cf_clearance in Settings.",
                cfChallenge = true
            )
        }

        if (status == 429 || lower.contains("error code: 1015") || lower.contains("too many requests")) {
            return FetchResult(
                ok = false,
                status = 429,
                body = body,
                error = "animepahe.pw rate-limited requests (HTTP 429 Too Many Requests).",
                cfChallenge = false
            )
        }

        if (status in 200..299) {
            return FetchResult(true, status, body, "", false)
        }

        return FetchResult(
            ok = false,
            status = status,
            body = body,
            error = "HTTP request failed with status code $status",
            cfChallenge = false
        )
    }

    @SuppressLint("SetJavaScriptEnabled", "AddJavascriptInterface")
    private fun ensureWebViewOnMain(origin: String, userAgent: String): WebView? {
        val holder = arrayOfNulls<WebView>(1)
        val latch = CountDownLatch(1)

        mainHandler.post {
            try {
                var wv = webViewsByOrigin[origin]
                if (wv == null) {
                    if (webViewsByOrigin.size >= MAX_WEBVIEWS) {
                        val oldestKey = webViewsByOrigin.keys.firstOrNull()
                        if (oldestKey != null) {
                            webViewsByOrigin.remove(oldestKey)?.destroy()
                            loadedOrigins.remove(oldestKey)
                        }
                    }

                    val ctx = AnimePaheApp.instance
                    wv = WebView(ctx)
                    val settings = wv.settings
                    settings.javaScriptEnabled = true
                    settings.domStorageEnabled = true
                    settings.databaseEnabled = true
                    settings.loadsImagesAutomatically = false
                    settings.blockNetworkImage = true
                    settings.allowFileAccess = false
                    settings.allowContentAccess = false
                    settings.cacheMode = WebSettings.LOAD_DEFAULT

                    val cm = CookieManager.getInstance()
                    cm.setAcceptCookie(true)
                    cm.setAcceptThirdPartyCookies(wv, true)

                    wv.addJavascriptInterface(BridgeJsInterface(), "__paheBridge")
                    wv.webViewClient = BridgeWebViewClient()
                    webViewsByOrigin[origin] = wv
                }

                val targetUa = if (userAgent.isNotBlank()) {
                    userAgent.trim()
                } else {
                    (wv.settings.userAgentString ?: "")
                        .replace("; wv", "")
                        .replace(Regex("Version/[0-9.]+\\s*"), "")
                }
                if (targetUa.isNotBlank() && wv.settings.userAgentString != targetUa) {
                    wv.settings.userAgentString = targetUa
                }

                holder[0] = wv
            } catch (t: Throwable) {
                Log.e(TAG, "Failed to create hidden WebView for $origin", t)
            } finally {
                latch.countDown()
            }
        }

        latch.await(5, TimeUnit.SECONDS)
        return holder[0]
    }

    private fun inspectLoadedPage(
        wv: WebView,
        nav: PendingNav,
        generation: Long,
        attemptStartMs: Long
    ) {
        if (nav.settled || activeNav !== nav || nav.pageGeneration != generation) return

        wv.evaluateJavascript(
            "(function(){return JSON.stringify({title: document.title || '', html: document.documentElement ? document.documentElement.outerHTML : ''});})();"
        ) { raw ->
            if (nav.settled || activeNav !== nav || nav.pageGeneration != generation) {
                return@evaluateJavascript
            }
            var title = ""
            var html = ""
            try {
                val unwrapped = unquoteEvalJson(raw)
                if (unwrapped.isNotEmpty()) {
                    val obj = JSONObject(unwrapped)
                    title = obj.optString("title", "")
                    html = obj.optString("html", "")
                }
            } catch (_: Throwable) {
            }

            val lowerTitle = title.lowercase()
            val lowerHtml = html.lowercase()
            val looksChallenge = lowerTitle.contains("just a moment") ||
                lowerTitle.contains("attention required") ||
                lowerTitle.contains("security check") ||
                isChallengeBody(lowerHtml)

            if (!looksChallenge) {
                nav.html = html
                nav.cfChallenge = false
                nav.settled = true
                nav.latch.countDown()
                return@evaluateJavascript
            }

            // Give Cloudflare's automatic JS check a short window to redirect if it can pass silently.
            val elapsed = System.currentTimeMillis() - attemptStartMs
            if (elapsed < CF_WAIT_MAX_MS) {
                mainHandler.postDelayed({
                    inspectLoadedPage(wv, nav, generation, attemptStartMs)
                }, 450)
            } else {
                nav.html = html
                nav.cfChallenge = true
                nav.settled = true
                nav.latch.countDown()
            }
        }
    }

    private class BridgeWebViewClient : WebViewClient() {
        override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest?): Boolean {
            return false
        }

        override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
            super.onPageStarted(view, url, favicon)
            val nav = activeNav
            if (nav != null && !nav.settled) {
                nav.pageGeneration += 1
                nav.httpStatus = 200
            }
        }

        override fun onReceivedHttpError(
            view: WebView?,
            request: WebResourceRequest?,
            errorResponse: WebResourceResponse?
        ) {
            super.onReceivedHttpError(view, request, errorResponse)
            if (request?.isForMainFrame == true) {
                val nav = activeNav
                val code = errorResponse?.statusCode ?: 0
                if (nav != null && !nav.settled && code > 0) {
                    nav.httpStatus = code
                }
            }
        }

        override fun onReceivedError(
            view: WebView?,
            request: WebResourceRequest?,
            error: WebResourceError?
        ) {
            super.onReceivedError(view, request, error)
            if (request?.isForMainFrame == true) {
                val nav = activeNav
                if (nav != null && !nav.settled) {
                    nav.netError = error?.description?.toString() ?: "WebView load error"
                    nav.settled = true
                    nav.latch.countDown()
                }
            }
        }

        override fun onPageFinished(view: WebView?, url: String?) {
            super.onPageFinished(view, url)
            if (view == null || url.isNullOrBlank() || url == "about:blank") return
            val nav = activeNav ?: return
            if (nav.settled) return
            inspectLoadedPage(view, nav, nav.pageGeneration, System.currentTimeMillis())
        }
    }

    private class BridgeJsInterface {
        @JavascriptInterface
        fun onFetchResult(reqId: String, status: Int, body: String?, error: String?) {
            val pending = activeFetch ?: return
            if (pending.reqId != reqId) return
            pending.status = status
            pending.body = body ?: ""
            pending.error = error ?: ""
            pending.latch.countDown()
        }
    }

    private fun syncManualCookieIfNeeded(origin: String, cfCookie: String) {
        val trimmed = cfCookie.trim()
        if (trimmed.isEmpty()) return
        val cm = CookieManager.getInstance()
        val existing = cm.getCookie(origin) ?: ""
        val existingClearance = extractClearance(existing)
        val normalizedInput = if (trimmed.contains("=")) trimmed else "cf_clearance=$trimmed"
        val incomingClearance = extractClearance(normalizedInput)
        if (incomingClearance.isNotEmpty() && incomingClearance == existingClearance) {
            return
        }
        for (part in normalizedInput.split(";")) {
            val kv = part.trim()
            if (kv.contains("=")) {
                cm.setCookie(origin, "$kv; Path=/; Secure")
            }
        }
        cm.flush()
        loadedOrigins.remove(origin)
    }

    private fun extractClearance(cookieStr: String?): String {
        if (cookieStr.isNullOrBlank()) return ""
        for (part in cookieStr.split(";")) {
            val trimmed = part.trim()
            if (trimmed.startsWith("cf_clearance=")) {
                return trimmed.removePrefix("cf_clearance=").trim().trim('"')
            }
        }
        return ""
    }

    private fun extractOrigin(rawUrl: String): String? {
        return try {
            val uri = Uri.parse(rawUrl)
            val scheme = uri.scheme ?: return null
            val authority = uri.encodedAuthority ?: return null
            "$scheme://$authority"
        } catch (_: Throwable) {
            null
        }
    }

    private fun isChallengeBody(lowerBody: String): Boolean {
        return lowerBody.contains("<title>just a moment") ||
            lowerBody.contains("cf-browser-verification") ||
            lowerBody.contains("_cf_chl_opt") ||
            lowerBody.contains("/cdn-cgi/challenge-platform")
    }

    private fun unquoteEvalJson(raw: String?): String {
        if (raw.isNullOrBlank() || raw == "null") return ""
        return try {
            JSONObject("{\"v\":$raw}").optString("v", "")
        } catch (_: Throwable) {
            raw
        }
    }

    private fun encodeResponse(
        ok: Boolean,
        status: Int,
        body: String,
        error: String,
        cfChallenge: Boolean
    ): String {
        return JSONObject().apply {
            put("ok", ok)
            put("status", status)
            put("body", body)
            put("error", error)
            put("cfChallenge", cfChallenge)
        }.toString()
    }
}
