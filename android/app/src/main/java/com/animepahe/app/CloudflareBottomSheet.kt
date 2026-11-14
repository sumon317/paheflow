package com.animepahe.app

import android.annotation.SuppressLint
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import com.google.android.material.bottomsheet.BottomSheetBehavior
import com.google.android.material.bottomsheet.BottomSheetDialog
import com.google.android.material.bottomsheet.BottomSheetDialogFragment

class CloudflareBottomSheet : BottomSheetDialogFragment() {

    companion object {
        const val TAG = "CloudflareBottomSheet"
        fun newInstance(onSuccess: () -> Unit): CloudflareBottomSheet {
            return CloudflareBottomSheet().apply {
                this.onSolvedCallback = onSuccess
            }
        }
    }

    var onSolvedCallback: (() -> Unit)? = null
    private var webView: WebView? = null
    private val handler = Handler(Looper.getMainLooper())
    private var isSolved = false
    private var initialClearance: String = ""
    private var targetOrigin: String = "https://animepahe.pw"
    private var isAnimepaheTarget: Boolean = true

    private val cookieChecker = object : Runnable {
        override fun run() {
            if (isSolved) return
            checkCookies(autoDismiss = true)
            handler.postDelayed(this, 800)
        }
    }

    override fun onStart() {
        super.onStart()
        val dialog = dialog as? BottomSheetDialog ?: return
        val bottomSheet = dialog.findViewById<View>(com.google.android.material.R.id.design_bottom_sheet) ?: return
        val behavior = BottomSheetBehavior.from(bottomSheet)
        behavior.state = BottomSheetBehavior.STATE_EXPANDED
        behavior.skipCollapsed = true
        val displayMetrics = resources.displayMetrics
        val height = (displayMetrics.heightPixels * 0.88).toInt()
        bottomSheet.layoutParams.height = height
    }

    override fun onCreateView(
        inflater: LayoutInflater,
        container: ViewGroup?,
        savedInstanceState: Bundle?
    ): View {
        val layout = LinearLayout(requireContext()).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 24, 32, 24)
            setBackgroundColor(0xFF161420.toInt())
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT
            )
        }

        val title = TextView(requireContext()).apply {
            text = getString(R.string.cf_solver_title)
            textSize = 17f
            setTextColor(0xFFFFFFFF.toInt())
            setTypeface(null, android.graphics.Typeface.BOLD)
            setPadding(0, 0, 0, 4)
        }

        val hint = TextView(requireContext()).apply {
            text = getString(R.string.cf_solver_hint)
            textSize = 12.5f
            setTextColor(0xFF9E9AA8.toInt())
            setPadding(0, 0, 0, 12)
        }

        val btnRow = LinearLayout(requireContext()).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, 0, 0, 12)
        }

        val btnDone = Button(requireContext()).apply {
            text = "Done / Save Cookie"
            setBackgroundColor(0xFF5352ED.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1.2f).apply {
                marginEnd = 6
            }
            setOnClickListener {
                checkCookies(autoDismiss = false, forceSave = true)
            }
        }

        val btnReload = Button(requireContext()).apply {
            text = "↻ Reload"
            setBackgroundColor(0xFF2E2C3D.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 0.8f).apply {
                marginEnd = 6
            }
            setOnClickListener {
                webView?.reload()
            }
        }

        val btnFirefox = Button(requireContext()).apply {
            text = "Firefox"
            setBackgroundColor(0xFF2E2C3D.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 0.9f)
            setOnClickListener {
                val loadUrl = "$targetOrigin/"
                val intent = Intent(Intent.ACTION_VIEW, Uri.parse(loadUrl)).apply {
                    setPackage("org.mozilla.firefox")
                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                }
                try {
                    startActivity(intent)
                } catch (_: Exception) {
                    val fallback = Intent(Intent.ACTION_VIEW, Uri.parse(loadUrl)).apply {
                        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    }
                    try {
                        startActivity(fallback)
                    } catch (_: Exception) {
                        Toast.makeText(
                            requireContext(),
                            "No browser app found to open AnimePahe",
                            Toast.LENGTH_SHORT
                        ).show()
                    }
                }
            }
        }

        btnRow.addView(btnDone)
        btnRow.addView(btnReload)
        btnRow.addView(btnFirefox)

        webView = WebView(requireContext()).apply {
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                0,
                1f
            )
        }

        layout.addView(title)
        layout.addView(hint)
        layout.addView(btnRow)
        layout.addView(webView)
        return layout
    }

    @SuppressLint("SetJavaScriptEnabled")
    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        val wv = webView ?: return

        val cookieManager = CookieManager.getInstance()
        cookieManager.setAcceptCookie(true)
        cookieManager.setAcceptThirdPartyCookies(wv, true)

        val pendingOrigin = AndroidHttpBridge.getPendingChallengeOrigin()?.trimEnd('/')
        if (!pendingOrigin.isNullOrBlank()) {
            targetOrigin = pendingOrigin
        } else {
            targetOrigin = "https://animepahe.pw"
        }
        val targetHost = Uri.parse(targetOrigin).host?.lowercase() ?: "animepahe.pw"
        isAnimepaheTarget = targetHost == "animepahe.pw" ||
            targetHost.endsWith(".animepahe.pw") ||
            targetHost == "animepahe.com" ||
            targetHost.endsWith(".animepahe.com") ||
            targetHost == "animepahe.org" ||
            targetHost.endsWith(".animepahe.org") ||
            targetHost == "animepahe.ru" ||
            targetHost.endsWith(".animepahe.ru") ||
            targetHost == "animepahe.si" ||
            targetHost.endsWith(".animepahe.si")

        // Capture whatever stale cookie might be in CookieManager currently
        val existingForTarget = if (isAnimepaheTarget) {
            val existingPw = cookieManager.getCookie("https://animepahe.pw") ?: ""
            val existingRu = cookieManager.getCookie("https://animepahe.ru") ?: ""
            if (existingPw.isNotEmpty()) existingPw else existingRu
        } else {
            cookieManager.getCookie(targetOrigin) ?: ""
        }
        initialClearance = extractClearance(existingForTarget)
        Log.d("CF_SHEET", "onViewCreated: target=$targetOrigin, initialClearance len=${initialClearance.length}")

        val settings = wv.settings
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.databaseEnabled = true
        settings.useWideViewPort = true
        settings.loadWithOverviewMode = true

        // Clean user agent: strip "; wv" and "Version/4.0 " so Cloudflare treats WebView as standard mobile Chrome
        val rawUa = settings.userAgentString ?: ""
        val cleanUa = rawUa
            .replace("; wv", "")
            .replace(Regex("Version/[0-9.]+\\s*"), "")
        settings.userAgentString = cleanUa

        wv.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest?): Boolean {
                return false
            }

            override fun onPageFinished(view: WebView?, url: String?) {
                super.onPageFinished(view, url)
                Log.d("CF_SHEET", "onPageFinished: $url, title: ${view?.title}")
                checkCookies(autoDismiss = true)
            }
        }

        var urlLoaded = false
        var fallbackRunnable: Runnable? = null
        val doLoad = {
            if (!urlLoaded && !isSolved && webView != null) {
                urlLoaded = true
                fallbackRunnable?.let { handler.removeCallbacks(it) }
                wv.loadUrl("$targetOrigin/")
                handler.postDelayed(cookieChecker, 1000)
            }
        }
        fallbackRunnable = Runnable { doLoad() }

        if (isAnimepaheTarget) {
            // Clear stale cookies first, then load AnimePahe cleanly
            cookieManager.removeAllCookies { success ->
                Log.d("CF_SHEET", "removeAllCookies result: $success")
                cookieManager.flush()
                wv.post { doLoad() }
            }
            handler.postDelayed(fallbackRunnable, 600)
        } else {
            // Keep animepahe.pw cookies intact while solving a stream-host challenge (e.g. kwik.cx)
            wv.post { doLoad() }
        }
    }

    private fun extractClearance(cookieStr: String?): String {
        if (cookieStr.isNullOrEmpty()) return ""
        for (part in cookieStr.split(";")) {
            val trimmed = part.trim()
            if (trimmed.startsWith("cf_clearance=")) {
                return trimmed.removePrefix("cf_clearance=").trim().trim('"')
            }
        }
        return ""
    }

    private fun checkCookies(autoDismiss: Boolean = true, forceSave: Boolean = false) {
        if (isSolved && !forceSave) return
        val cookieManager = CookieManager.getInstance()
        val clearance = if (isAnimepaheTarget) {
            val cookiesPwSlash = cookieManager.getCookie("https://animepahe.pw/") ?: ""
            val cookiesPw = cookieManager.getCookie("https://animepahe.pw") ?: ""
            val cookiesRuSlash = cookieManager.getCookie("https://animepahe.ru/") ?: ""
            val cookiesRu = cookieManager.getCookie("https://animepahe.ru") ?: ""
            val cookiesOrg = cookieManager.getCookie("https://animepahe.org/") ?: ""
            listOf(
                extractClearance(cookiesPwSlash),
                extractClearance(cookiesPw),
                extractClearance(cookiesRuSlash),
                extractClearance(cookiesRu),
                extractClearance(cookiesOrg)
            ).firstOrNull { it.isNotEmpty() } ?: ""
        } else {
            val cookiesTargetSlash = cookieManager.getCookie("$targetOrigin/") ?: ""
            val cookiesTarget = cookieManager.getCookie(targetOrigin) ?: ""
            listOf(
                extractClearance(cookiesTargetSlash),
                extractClearance(cookiesTarget)
            ).firstOrNull { it.isNotEmpty() } ?: ""
        }

        val wvTitle = webView?.title?.lowercase() ?: ""
        val isChallengeTitle = wvTitle.contains("just a moment") ||
                wvTitle.contains("attention required") ||
                wvTitle.contains("security check") ||
                wvTitle.contains("cloudflare")

        val currentUrl = webView?.url?.lowercase() ?: ""
        val targetHost = Uri.parse(targetOrigin).host?.lowercase() ?: "animepahe.pw"
        val onSite = if (isAnimepaheTarget) {
            currentUrl.contains("animepahe.pw") ||
                currentUrl.contains("animepahe.ru") ||
                currentUrl.contains("animepahe.org") ||
                currentUrl.contains("animepahe.si")
        } else {
            currentUrl.contains(targetHost)
        }

        val challengePassed = !isChallengeTitle && onSite && currentUrl.isNotBlank()

        if (forceSave) {
            if (isAnimepaheTarget && clearance.isEmpty()) {
                Toast.makeText(
                    requireContext(),
                    "No cf_clearance cookie detected yet. Please solve the challenge above.",
                    Toast.LENGTH_SHORT
                ).show()
                return
            }
            if (!isAnimepaheTarget && !challengePassed) {
                Toast.makeText(
                    requireContext(),
                    "Please wait for the Cloudflare check to finish loading.",
                    Toast.LENGTH_SHORT
                ).show()
                return
            }
            saveAndDismiss(clearance)
            return
        }

        // Auto-dismiss once clearance cookie is detected and page challenge is passed
        val readyToDismiss = if (isAnimepaheTarget) {
            clearance.isNotEmpty() && challengePassed
        } else {
            challengePassed && (clearance.isNotEmpty() || wvTitle.contains("kwik"))
        }
        if (autoDismiss && readyToDismiss) {
            Log.d("CF_SHEET", "Challenge verified for $targetOrigin! Auto-dismissing.")
            saveAndDismiss(clearance)
        }
    }

    private fun saveAndDismiss(clearanceVal: String) {
        Log.d("CF_SHEET", "saveAndDismiss: target=$targetOrigin, clearance len=${clearanceVal.length}")
        isSolved = true
        handler.removeCallbacks(cookieChecker)
        CookieManager.getInstance().flush()
        AndroidHttpBridge.clearPendingChallengeOrigin()
        AndroidHttpBridge.onCredentialsUpdated()
        if (isAnimepaheTarget && clearanceVal.isNotEmpty()) {
            val ua = webView?.settings?.userAgentString ?: ""
            NativeEngine.notifyCloudflareSolved(clearanceVal, ua)
        }
        Toast.makeText(requireContext(), "Cloudflare clearance updated!", Toast.LENGTH_SHORT).show()
        onSolvedCallback?.invoke()
        dismissAllowingStateLoss()
    }

    override fun onDestroyView() {
        Log.d("CF_SHEET", "onDestroyView")
        handler.removeCallbacks(cookieChecker)
        webView?.destroy()
        webView = null
        super.onDestroyView()
    }
}
