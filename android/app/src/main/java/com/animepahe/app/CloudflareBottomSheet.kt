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
                val intent = Intent(Intent.ACTION_VIEW, Uri.parse("https://animepahe.pw/")).apply {
                    setPackage("org.mozilla.firefox")
                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                }
                try {
                    startActivity(intent)
                } catch (_: Exception) {
                    val fallback = Intent(Intent.ACTION_VIEW, Uri.parse("https://animepahe.pw/")).apply {
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

        // Capture whatever stale cookie might be in CookieManager currently
        val existingPw = cookieManager.getCookie("https://animepahe.pw") ?: ""
        val existingRu = cookieManager.getCookie("https://animepahe.ru") ?: ""
        initialClearance = extractClearance(if (existingPw.isNotEmpty()) existingPw else existingRu)
        Log.d("CF_SHEET", "onViewCreated: initialClearance len=${initialClearance.length}")

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
        val doLoad = {
            if (!urlLoaded && !isSolved && webView != null) {
                urlLoaded = true
                wv.loadUrl("https://animepahe.pw/")
                handler.postDelayed(cookieChecker, 1000)
            }
        }

        // Clear stale cookies first, then load AnimePahe cleanly
        cookieManager.removeAllCookies { success ->
            Log.d("CF_SHEET", "removeAllCookies result: $success")
            cookieManager.flush()
            wv.post { doLoad() }
        }
        handler.postDelayed({ doLoad() }, 600)
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
        val cookiesPwSlash = cookieManager.getCookie("https://animepahe.pw/") ?: ""
        val cookiesPw = cookieManager.getCookie("https://animepahe.pw") ?: ""
        val cookiesRuSlash = cookieManager.getCookie("https://animepahe.ru/") ?: ""
        val cookiesRu = cookieManager.getCookie("https://animepahe.ru") ?: ""
        val cookiesOrg = cookieManager.getCookie("https://animepahe.org/") ?: ""

        val clearance = listOf(
            extractClearance(cookiesPwSlash),
            extractClearance(cookiesPw),
            extractClearance(cookiesRuSlash),
            extractClearance(cookiesRu),
            extractClearance(cookiesOrg)
        ).firstOrNull { it.isNotEmpty() } ?: ""

        val wvTitle = webView?.title?.lowercase() ?: ""
        val isChallengeTitle = wvTitle.contains("just a moment") ||
                wvTitle.contains("attention required") ||
                wvTitle.contains("security check") ||
                wvTitle.contains("cloudflare")

        val currentUrl = webView?.url?.lowercase() ?: ""
        val onSite = currentUrl.contains("animepahe.pw") ||
                currentUrl.contains("animepahe.ru") ||
                currentUrl.contains("animepahe.org")

        val challengePassed = !isChallengeTitle && onSite

        if (forceSave) {
            if (clearance.isEmpty()) {
                Toast.makeText(
                    requireContext(),
                    "No cf_clearance cookie detected yet. Please solve the challenge above.",
                    Toast.LENGTH_SHORT
                ).show()
                return
            }
            saveAndDismiss(clearance)
            return
        }

        // Auto-dismiss once clearance cookie is detected and page challenge is passed
        if (autoDismiss && clearance.isNotEmpty() && challengePassed) {
            Log.d("CF_SHEET", "Clearance cookie verified! Auto-dismissing.")
            saveAndDismiss(clearance)
        }
    }

    private fun saveAndDismiss(clearanceVal: String) {
        Log.d("CF_SHEET", "saveAndDismiss: clearance len=${clearanceVal.length}")
        isSolved = true
        handler.removeCallbacks(cookieChecker)
        val ua = webView?.settings?.userAgentString ?: ""
        NativeEngine.notifyCloudflareSolved(clearanceVal, ua)
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
