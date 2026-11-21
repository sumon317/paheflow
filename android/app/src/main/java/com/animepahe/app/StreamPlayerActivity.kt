package com.animepahe.app

import android.annotation.SuppressLint
import android.app.PictureInPictureParams
import android.content.Context
import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Rational
import android.view.GestureDetector
import android.view.MotionEvent
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.annotation.OptIn
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.lifecycleScope
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MimeTypes
import androidx.media3.common.PlaybackException
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.exoplayer.DefaultLoadControl
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.hls.HlsMediaSource
import androidx.media3.ui.AspectRatioFrameLayout
import androidx.media3.ui.PlayerView
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder

@OptIn(UnstableApi::class)
class StreamPlayerActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_SLUG = "extra_slug"
        const val EXTRA_TITLE = "extra_title"
        const val EXTRA_EPISODE = "extra_episode"
        const val EXTRA_SESSION = "extra_session"
        const val EXTRA_RESOLUTION = "extra_resolution"
        const val EXTRA_AUDIO = "extra_audio"
        const val EXTRA_EPISODES_JSON = "extra_episodes_json"

        private const val PREFS_PROGRESS = "paheflow_stream_progress"
        private const val SKIP_INTRO_MS = 85_000L
        private const val DOUBLE_TAP_SEEK_MS = 10_000L
        private val SPEED_OPTIONS = floatArrayOf(0.5f, 0.75f, 1.0f, 1.25f, 1.5f, 1.75f, 2.0f)

        fun createIntent(
            context: Context,
            slug: String,
            title: String,
            episode: String,
            session: String = "",
            resolution: String = "",
            audio: String = "",
            episodesJson: String = "[]"
        ): Intent {
            return Intent(context, StreamPlayerActivity::class.java).apply {
                putExtra(EXTRA_SLUG, slug)
                putExtra(EXTRA_TITLE, title)
                putExtra(EXTRA_EPISODE, episode)
                putExtra(EXTRA_SESSION, session)
                putExtra(EXTRA_RESOLUTION, resolution)
                putExtra(EXTRA_AUDIO, audio)
                putExtra(EXTRA_EPISODES_JSON, episodesJson)
            }
        }
    }

    private data class EpisodeRef(
        val episode: String,
        val session: String,
        val audio: String
    )

    private data class VariantInfo(
        val resolution: String,
        val audio: String,
        val av1: Boolean,
        val filesize: String,
        val selected: Boolean
    )

    private data class ResolveResult(
        val ok: Boolean,
        val streamId: String = "",
        val playlistUrl: String = "",
        val slug: String = "",
        val title: String = "",
        val episode: String = "",
        val session: String = "",
        val resolution: String = "",
        val audio: String = "",
        val filesize: String = "",
        val variants: List<VariantInfo> = emptyList(),
        val error: String = "",
        val cfChallenge: Boolean = false
    )

    private lateinit var playerView: PlayerView
    private lateinit var topOverlay: LinearLayout
    private lateinit var quickActionsBar: LinearLayout
    private lateinit var loadingOverlay: LinearLayout
    private lateinit var errorOverlay: LinearLayout
    private lateinit var tvAnimeTitle: TextView
    private lateinit var tvEpisodeMeta: TextView
    private lateinit var tvLoadingStatus: TextView
    private lateinit var tvErrorMessage: TextView
    private lateinit var tvSeekFeedback: TextView
    private lateinit var btnBack: TextView
    private lateinit var btnQuality: TextView
    private lateinit var btnSpeed: TextView
    private lateinit var btnResize: TextView
    private lateinit var btnRotate: TextView
    private lateinit var btnPip: TextView
    private lateinit var btnPrevEpisode: TextView
    private lateinit var btnNextEpisode: TextView
    private lateinit var btnSkipIntro: TextView
    private lateinit var btnSolveCf: Button
    private lateinit var btnRetry: Button
    private lateinit var btnCloseError: Button

    private var player: ExoPlayer? = null
    private var resolveJob: Job? = null
    private var seekFeedbackJob: Job? = null

    private var slug: String = ""
    private var animeTitle: String = ""
    private var currentEpisode: String = ""
    private var currentSession: String = ""
    private var preferredResolution: String = ""
    private var preferredAudio: String = ""
    private var activeStreamId: String = ""
    private var activeResolution: String = ""
    private var activeAudio: String = ""
    private var variants: List<VariantInfo> = emptyList()
    private var episodeList: List<EpisodeRef> = emptyList()

    private var currentSpeed: Float = 1.0f
    private var resizeModeIndex: Int = 0
    private var isPortraitLocked: Boolean = false
    private var resumePositionMs: Long = 0L
    private var playWhenReadyState: Boolean = true
    private var inPipMode: Boolean = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_stream_player)

        bindViews()
        hideSystemBars()
        readIntentParams(intent)
        setupControls()
        initPlayer()
        loadCurrentEpisode(preservePositionMs = null)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        saveWatchProgress()
        readIntentParams(intent)
        loadCurrentEpisode(preservePositionMs = null)
    }

    private fun bindViews() {
        playerView = findViewById(R.id.playerView)
        topOverlay = findViewById(R.id.topOverlay)
        quickActionsBar = findViewById(R.id.quickActionsBar)
        loadingOverlay = findViewById(R.id.loadingOverlay)
        errorOverlay = findViewById(R.id.errorOverlay)
        tvAnimeTitle = findViewById(R.id.tvAnimeTitle)
        tvEpisodeMeta = findViewById(R.id.tvEpisodeMeta)
        tvLoadingStatus = findViewById(R.id.tvLoadingStatus)
        tvErrorMessage = findViewById(R.id.tvErrorMessage)
        tvSeekFeedback = findViewById(R.id.tvSeekFeedback)
        btnBack = findViewById(R.id.btnBack)
        btnQuality = findViewById(R.id.btnQuality)
        btnSpeed = findViewById(R.id.btnSpeed)
        btnResize = findViewById(R.id.btnResize)
        btnRotate = findViewById(R.id.btnRotate)
        btnPip = findViewById(R.id.btnPip)
        btnPrevEpisode = findViewById(R.id.btnPrevEpisode)
        btnNextEpisode = findViewById(R.id.btnNextEpisode)
        btnSkipIntro = findViewById(R.id.btnSkipIntro)
        btnSolveCf = findViewById(R.id.btnSolveCf)
        btnRetry = findViewById(R.id.btnRetry)
        btnCloseError = findViewById(R.id.btnCloseError)

        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            btnPip.visibility = View.GONE
        }
    }

    private fun readIntentParams(intent: Intent) {
        slug = intent.getStringExtra(EXTRA_SLUG)?.trim().orEmpty()
        animeTitle = intent.getStringExtra(EXTRA_TITLE)?.trim().orEmpty().ifEmpty { slug }
        currentEpisode = intent.getStringExtra(EXTRA_EPISODE)?.trim().orEmpty()
        currentSession = intent.getStringExtra(EXTRA_SESSION)?.trim().orEmpty()
        preferredResolution = intent.getStringExtra(EXTRA_RESOLUTION)?.trim().orEmpty()
        preferredAudio = intent.getStringExtra(EXTRA_AUDIO)?.trim().orEmpty()
        episodeList = parseEpisodeList(intent.getStringExtra(EXTRA_EPISODES_JSON).orEmpty())
        updateHeaderUi()
        updateEpisodeNavButtons()
    }

    private fun parseEpisodeList(raw: String): List<EpisodeRef> {
        if (raw.isBlank()) return emptyList()
        return try {
            val arr = JSONArray(raw)
            val list = ArrayList<EpisodeRef>(arr.length())
            for (i in 0 until arr.length()) {
                val obj = arr.optJSONObject(i) ?: continue
                val ep = obj.optString("episode", "").trim()
                if (ep.isNotEmpty()) {
                    list.add(
                        EpisodeRef(
                            episode = ep,
                            session = obj.optString("session", "").trim(),
                            audio = obj.optString("audio", "").trim()
                        )
                    )
                }
            }
            list
        } catch (_: Exception) {
            emptyList()
        }
    }

    @SuppressLint("ClickableViewAccessibility")
    private fun setupControls() {
        btnBack.setOnClickListener { finish() }
        btnCloseError.setOnClickListener { finish() }

        btnRetry.setOnClickListener {
            val pos = player?.currentPosition?.takeIf { it > 0 }
            loadCurrentEpisode(preservePositionMs = pos)
        }

        btnSolveCf.setOnClickListener {
            openCloudflareSolver()
        }

        btnQuality.setOnClickListener {
            showQualityDialog()
        }

        btnSpeed.setOnClickListener {
            showSpeedDialog()
        }

        btnResize.setOnClickListener {
            cycleResizeMode()
        }

        btnRotate.setOnClickListener {
            isPortraitLocked = !isPortraitLocked
            requestedOrientation = if (isPortraitLocked) {
                ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
            } else {
                ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
            }
        }

        btnPip.setOnClickListener {
            enterPipIfSupported()
        }

        btnSkipIntro.setOnClickListener {
            seekRelative(SKIP_INTRO_MS, "+85s")
        }

        btnPrevEpisode.setOnClickListener {
            switchEpisodeByOffset(-1)
        }

        btnNextEpisode.setOnClickListener {
            switchEpisodeByOffset(1)
        }

        playerView.setControllerVisibilityListener(
            PlayerView.ControllerVisibilityListener { visibility ->
                val show = visibility == View.VISIBLE && !inPipMode &&
                    loadingOverlay.visibility != View.VISIBLE &&
                    errorOverlay.visibility != View.VISIBLE
                topOverlay.visibility = if (show) View.VISIBLE else View.GONE
                quickActionsBar.visibility = if (show) View.VISIBLE else View.GONE
                if (!show) {
                    hideSystemBars()
                }
            }
        )

        val gestureDetector = GestureDetector(
            this,
            object : GestureDetector.SimpleOnGestureListener() {
                override fun onDoubleTap(e: MotionEvent): Boolean {
                    val width = playerView.width
                    if (width <= 0) return false
                    return when {
                        e.x < width * 0.35f -> {
                            seekRelative(-DOUBLE_TAP_SEEK_MS, "« 10s")
                            true
                        }
                        e.x > width * 0.65f -> {
                            seekRelative(DOUBLE_TAP_SEEK_MS, "10s »")
                            true
                        }
                        else -> {
                            val p = player ?: return false
                            if (p.isPlaying) p.pause() else p.play()
                            true
                        }
                    }
                }
            }
        )

        playerView.setOnTouchListener { _, event ->
            gestureDetector.onTouchEvent(event)
            false
        }
    }

    private fun initPlayer() {
        if (player != null) return

        val loadControl = DefaultLoadControl.Builder()
            .setBufferDurationsMs(
                /* minBufferMs = */ 30_000,
                /* maxBufferMs = */ 120_000,
                /* bufferForPlaybackMs = */ 1_000,
                /* bufferForPlaybackAfterRebufferMs = */ 2_000
            )
            .build()

        val audioAttributes = AudioAttributes.Builder()
            .setUsage(C.USAGE_MEDIA)
            .setContentType(C.AUDIO_CONTENT_TYPE_MOVIE)
            .build()

        val exo = ExoPlayer.Builder(this)
            .setLoadControl(loadControl)
            .build().apply {
                setAudioAttributes(audioAttributes, true)
                setHandleAudioBecomingNoisy(true)
                playbackParameters = PlaybackParameters(currentSpeed)
                addListener(object : Player.Listener {
                    override fun onPlaybackStateChanged(playbackState: Int) {
                        when (playbackState) {
                            Player.STATE_READY -> {
                                loadingOverlay.visibility = View.GONE
                                errorOverlay.visibility = View.GONE
                            }
                            Player.STATE_ENDED -> {
                                clearWatchProgress()
                                if (findEpisodeIndex() in 0 until episodeList.size - 1) {
                                    playerView.showController()
                                    Toast.makeText(
                                        this@StreamPlayerActivity,
                                        "Episode finished — tap Next ⏭ for the next episode",
                                        Toast.LENGTH_SHORT
                                    ).show()
                                }
                            }
                            else -> {}
                        }
                    }

                    override fun onPlayerError(error: PlaybackException) {
                        val pos = currentPosition.takeIf { it > 0 }
                        if (pos != null) {
                            resumePositionMs = pos
                        }
                        val detail = error.cause?.localizedMessage ?: error.localizedMessage ?: "Stream playback failed"
                        showError(
                            "Playback interrupted: $detail\n\nTap Retry to refresh the stream link.",
                            cfChallenge = false
                        )
                    }
                })
            }

        player = exo
        playerView.player = exo
    }

    private fun loadCurrentEpisode(preservePositionMs: Long?) {
        resolveJob?.cancel()
        val previousStreamId = activeStreamId
        activeStreamId = ""
        if (previousStreamId.isNotEmpty()) {
            closeStreamSessionAsync(previousStreamId)
        }

        player?.stop()
        player?.clearMediaItems()

        showLoading("Resolving Episode $currentEpisode…")
        updateHeaderUi()
        updateEpisodeNavButtons()

        val targetResumeMs = preservePositionMs ?: loadSavedWatchProgress()
        resumePositionMs = targetResumeMs

        resolveJob = lifecycleScope.launch {
            try {
                val result = withContext(Dispatchers.IO) {
                    resolveStreamFromLocalServer(
                        slug = slug,
                        title = animeTitle,
                        episode = currentEpisode,
                        session = currentSession,
                        resolution = preferredResolution,
                        audio = preferredAudio
                    )
                }

                if (!result.ok) {
                    showError(
                        result.error.ifBlank { "Could not resolve stream for Episode $currentEpisode" },
                        cfChallenge = result.cfChallenge
                    )
                    return@launch
                }

                if (result.slug.isNotEmpty()) slug = result.slug
                if (result.title.isNotEmpty()) animeTitle = result.title
                if (result.session.isNotEmpty()) currentSession = result.session
                activeStreamId = result.streamId
                activeResolution = result.resolution
                activeAudio = result.audio
                variants = result.variants

                updateHeaderUi()
                startHlsPlayback(result.playlistUrl, targetResumeMs)
            } catch (_: CancellationException) {
                // Superseded by a newer request or activity close
            } catch (e: Exception) {
                showError("Unexpected error: ${e.message ?: "Unknown"}", cfChallenge = false)
            }
        }
    }

    private fun startHlsPlayback(relativePlaylistUrl: String, seekToMs: Long) {
        val port = NativeEngine.getServerPort()
        val token = NativeEngine.getServerToken()
        if (port <= 0 || token.isBlank()) {
            showError("Local streaming engine is not running.", cfChallenge = false)
            return
        }

        val encodedToken = URLEncoder.encode(token, "UTF-8")
        val separator = if (relativePlaylistUrl.contains("?")) "&" else "?"
        val fullUrl = "http://127.0.0.1:$port$relativePlaylistUrl${separator}token=$encodedToken"

        val dataSourceFactory = DefaultHttpDataSource.Factory()
            .setConnectTimeoutMs(20_000)
            .setReadTimeoutMs(30_000)
            .setAllowCrossProtocolRedirects(false)
            .setDefaultRequestProperties(mapOf("X-App-Token" to token))

        val mediaItem = MediaItem.Builder()
            .setUri(Uri.parse(fullUrl))
            .setMimeType(MimeTypes.APPLICATION_M3U8)
            .build()

        val hlsSource = HlsMediaSource.Factory(dataSourceFactory)
            .setAllowChunklessPreparation(true)
            .createMediaSource(mediaItem)

        initPlayer()
        val exo = player ?: return
        exo.setMediaSource(hlsSource)
        if (seekToMs > 0L) {
            exo.seekTo(seekToMs)
        }
        exo.playbackParameters = PlaybackParameters(currentSpeed)
        exo.playWhenReady = true
        exo.prepare()
        tvLoadingStatus.text = "Buffering Episode $currentEpisode…"
    }

    private fun resolveStreamFromLocalServer(
        slug: String,
        title: String,
        episode: String,
        session: String,
        resolution: String,
        audio: String
    ): ResolveResult {
        val port = NativeEngine.getServerPort()
        val token = NativeEngine.getServerToken()
        if (port <= 0 || token.isBlank()) {
            return ResolveResult(ok = false, error = "Local PaheFlow server is not ready yet.")
        }

        val conn = (URL("http://127.0.0.1:$port/api/stream/resolve").openConnection() as HttpURLConnection).apply {
            requestMethod = "POST"
            connectTimeout = 15_000
            readTimeout = 45_000
            doOutput = true
            setRequestProperty("Content-Type", "application/json; charset=utf-8")
            setRequestProperty("X-App-Token", token)
        }

        return try {
            val bodyJson = JSONObject().apply {
                put("slug", slug)
                put("title", title)
                put("episode", episode)
                if (session.isNotEmpty()) put("session", session)
                if (resolution.isNotEmpty()) put("resolution", resolution)
                if (audio.isNotEmpty()) put("audio", audio)
            }.toString()

            conn.outputStream.use { it.write(bodyJson.toByteArray(Charsets.UTF_8)) }

            val code = conn.responseCode
            val stream = if (code in 200..299) conn.inputStream else conn.errorStream
            val responseText = stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() }.orEmpty()
            val json = try {
                JSONObject(responseText)
            } catch (_: Exception) {
                JSONObject()
            }

            if (code !in 200..299 || !json.optBoolean("ok", false)) {
                val errMsg = json.optString("error", "Stream resolve failed (HTTP $code)")
                val cf = json.optBoolean("cfChallenge", code == 403)
                return ResolveResult(ok = false, error = errMsg, cfChallenge = cf)
            }

            val parsedVariants = mutableListOf<VariantInfo>()
            val varArr = json.optJSONArray("variants")
            if (varArr != null) {
                for (i in 0 until varArr.length()) {
                    val v = varArr.optJSONObject(i) ?: continue
                    parsedVariants.add(
                        VariantInfo(
                            resolution = v.optString("resolution", ""),
                            audio = v.optString("audio", ""),
                            av1 = v.optBoolean("av1", false),
                            filesize = v.optString("filesize", ""),
                            selected = v.optBoolean("selected", false)
                        )
                    )
                }
            }

            ResolveResult(
                ok = true,
                streamId = json.optString("streamId", ""),
                playlistUrl = json.optString("playlistUrl", ""),
                slug = json.optString("slug", slug),
                title = json.optString("title", title),
                episode = json.optString("episode", episode),
                session = json.optString("session", session),
                resolution = json.optString("resolution", ""),
                audio = json.optString("audio", ""),
                filesize = json.optString("filesize", ""),
                variants = parsedVariants
            )
        } catch (e: Exception) {
            ResolveResult(ok = false, error = "Could not connect to local server: ${e.message}")
        } finally {
            conn.disconnect()
        }
    }

    private fun closeStreamSessionAsync(streamId: String) {
        val port = NativeEngine.getServerPort()
        val token = NativeEngine.getServerToken()
        if (port <= 0 || token.isBlank() || streamId.isBlank()) return
        // Use a lightweight daemon thread so session cleanup still completes when invoked from onDestroy(),
        // where lifecycleScope is already being cancelled.
        Thread({
            try {
                val conn = (URL("http://127.0.0.1:$port/api/stream/close").openConnection() as HttpURLConnection).apply {
                    requestMethod = "POST"
                    connectTimeout = 5_000
                    readTimeout = 5_000
                    doOutput = true
                    setRequestProperty("Content-Type", "application/json; charset=utf-8")
                    setRequestProperty("X-App-Token", token)
                }
                try {
                    val payload = JSONObject().put("streamId", streamId).toString()
                    conn.outputStream.use { it.write(payload.toByteArray(Charsets.UTF_8)) }
                    conn.responseCode
                } finally {
                    conn.disconnect()
                }
            } catch (_: Exception) {
            }
        }, "paheflow-stream-close").apply {
            isDaemon = true
            start()
        }
    }

    private fun openCloudflareSolver() {
        if (isFinishing || isDestroyed) return
        val existing = supportFragmentManager.findFragmentByTag(CloudflareBottomSheet.TAG)
        if (existing != null && existing.isAdded) return
        val sheet = CloudflareBottomSheet.newInstance {
            val pos = player?.currentPosition?.takeIf { it > 0 } ?: resumePositionMs
            loadCurrentEpisode(preservePositionMs = pos)
        }
        sheet.show(supportFragmentManager, CloudflareBottomSheet.TAG)
    }

    private fun showQualityDialog() {
        if (variants.isEmpty()) {
            Toast.makeText(this, "No alternate qualities available for this episode", Toast.LENGTH_SHORT).show()
            return
        }
        val labels = variants.map { v ->
            val audioLabel = formatAudioLabel(v.audio)
            val sizePart = if (v.filesize.isNotBlank()) " (${v.filesize})" else ""
            val codecPart = if (v.av1) " [AV1]" else ""
            val check = if (v.selected) "  ✓" else ""
            "${v.resolution}p · $audioLabel$sizePart$codecPart$check"
        }.toTypedArray()

        AlertDialog.Builder(this)
            .setTitle("Quality & Audio")
            .setItems(labels) { _, which ->
                val picked = variants.getOrNull(which) ?: return@setItems
                if (picked.resolution == activeResolution && picked.audio.equals(activeAudio, ignoreCase = true)) {
                    return@setItems
                }
                preferredResolution = picked.resolution
                preferredAudio = picked.audio
                val currentPos = player?.currentPosition?.takeIf { it > 0 } ?: 0L
                loadCurrentEpisode(preservePositionMs = currentPos)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun showSpeedDialog() {
        val labels = SPEED_OPTIONS.map { s ->
            val label = if (s == 1.0f) "1.0x (Normal)" else "${s}x"
            if (kotlin.math.abs(s - currentSpeed) < 0.01f) "$label  ✓" else label
        }.toTypedArray()

        AlertDialog.Builder(this)
            .setTitle("Playback Speed")
            .setItems(labels) { _, which ->
                currentSpeed = SPEED_OPTIONS[which]
                player?.playbackParameters = PlaybackParameters(currentSpeed)
                btnSpeed.text = "${currentSpeed}x"
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun cycleResizeMode() {
        resizeModeIndex = (resizeModeIndex + 1) % 3
        val (mode, label) = when (resizeModeIndex) {
            0 -> AspectRatioFrameLayout.RESIZE_MODE_FIT to "Fit"
            1 -> AspectRatioFrameLayout.RESIZE_MODE_ZOOM to "Zoom"
            else -> AspectRatioFrameLayout.RESIZE_MODE_FILL to "Fill"
        }
        playerView.resizeMode = mode
        btnResize.text = label
    }

    private fun seekRelative(deltaMs: Long, feedbackLabel: String) {
        val exo = player ?: return
        val duration = exo.duration
        val target = (exo.currentPosition + deltaMs).let { pos ->
            when {
                pos < 0L -> 0L
                duration > 0L && pos > duration -> duration
                else -> pos
            }
        }
        exo.seekTo(target)
        showSeekFeedback(feedbackLabel)
    }

    private fun showSeekFeedback(label: String) {
        seekFeedbackJob?.cancel()
        tvSeekFeedback.text = label
        tvSeekFeedback.visibility = View.VISIBLE
        seekFeedbackJob = lifecycleScope.launch {
            delay(650)
            tvSeekFeedback.visibility = View.GONE
        }
    }

    private fun findEpisodeIndex(): Int {
        return episodeList.indexOfFirst { it.episode.equals(currentEpisode, ignoreCase = true) }
    }

    private fun updateEpisodeNavButtons() {
        val idx = findEpisodeIndex()
        btnPrevEpisode.visibility = if (idx > 0) View.VISIBLE else View.GONE
        btnNextEpisode.visibility = if (idx in 0 until episodeList.size - 1) View.VISIBLE else View.GONE
    }

    private fun switchEpisodeByOffset(offset: Int) {
        val idx = findEpisodeIndex()
        if (idx < 0) return
        val next = episodeList.getOrNull(idx + offset) ?: return
        saveWatchProgress()
        currentEpisode = next.episode
        currentSession = next.session
        loadCurrentEpisode(preservePositionMs = null)
    }

    private fun updateHeaderUi() {
        tvAnimeTitle.text = animeTitle.ifBlank { "PaheFlow" }
        val parts = mutableListOf<String>()
        if (currentEpisode.isNotBlank()) parts.add("Episode $currentEpisode")
        if (activeResolution.isNotBlank()) parts.add("${activeResolution}p")
        if (activeAudio.isNotBlank()) parts.add(formatAudioLabel(activeAudio))
        tvEpisodeMeta.text = parts.joinToString(" · ").ifEmpty { "Loading…" }

        btnQuality.text = when {
            activeResolution.isNotBlank() && activeAudio.isNotBlank() ->
                "${activeResolution}p ${formatAudioLabel(activeAudio)}"
            activeResolution.isNotBlank() -> "${activeResolution}p"
            else -> "Quality"
        }
    }

    private fun formatAudioLabel(audio: String): String {
        return when (audio.trim().lowercase()) {
            "jpn", "sub" -> "SUB"
            "eng", "dub" -> "DUB"
            "" -> ""
            else -> audio.trim().uppercase()
        }
    }

    private fun showLoading(message: String) {
        tvLoadingStatus.text = message
        loadingOverlay.visibility = View.VISIBLE
        errorOverlay.visibility = View.GONE
        topOverlay.visibility = View.VISIBLE
        quickActionsBar.visibility = View.GONE
    }

    private fun showError(message: String, cfChallenge: Boolean) {
        loadingOverlay.visibility = View.GONE
        errorOverlay.visibility = View.VISIBLE
        topOverlay.visibility = View.VISIBLE
        quickActionsBar.visibility = View.GONE
        tvErrorMessage.text = message
        btnSolveCf.visibility = if (cfChallenge) View.VISIBLE else View.GONE
    }

    private fun progressKey(): String = "${slug}__${currentEpisode}"

    private fun saveWatchProgress() {
        val exo = player ?: return
        if (slug.isBlank() || currentEpisode.isBlank()) return
        val pos = exo.currentPosition
        val dur = exo.duration
        val prefs = getSharedPreferences(PREFS_PROGRESS, MODE_PRIVATE)
        if (dur > 0 && pos >= dur - 15_000L) {
            prefs.edit().remove(progressKey()).apply()
        } else if (pos > 5_000L) {
            prefs.edit().putLong(progressKey(), pos).apply()
        }
    }

    private fun loadSavedWatchProgress(): Long {
        if (slug.isBlank() || currentEpisode.isBlank()) return 0L
        val prefs = getSharedPreferences(PREFS_PROGRESS, MODE_PRIVATE)
        return prefs.getLong(progressKey(), 0L)
    }

    private fun clearWatchProgress() {
        if (slug.isBlank() || currentEpisode.isBlank()) return
        getSharedPreferences(PREFS_PROGRESS, MODE_PRIVATE)
            .edit()
            .remove(progressKey())
            .apply()
    }

    private fun enterPipIfSupported() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        try {
            val videoSize = player?.videoSize
            val ratio = if (videoSize != null && videoSize.width > 0 && videoSize.height > 0) {
                val r = videoSize.width.toFloat() / videoSize.height.toFloat()
                if (r in 0.45f..2.35f) Rational(videoSize.width, videoSize.height) else Rational(16, 9)
            } else {
                Rational(16, 9)
            }
            val params = PictureInPictureParams.Builder()
                .setAspectRatio(ratio)
                .build()
            enterPictureInPictureMode(params)
        } catch (e: Exception) {
            Toast.makeText(this, "PiP unavailable: ${e.message}", Toast.LENGTH_SHORT).show()
        }
    }

    override fun onPictureInPictureModeChanged(
        isInPictureInPictureMode: Boolean,
        newConfig: Configuration
    ) {
        super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
        inPipMode = isInPictureInPictureMode
        if (isInPictureInPictureMode) {
            topOverlay.visibility = View.GONE
            quickActionsBar.visibility = View.GONE
            playerView.useController = false
        } else {
            playerView.useController = true
            hideSystemBars()
        }
    }

    override fun onPause() {
        super.onPause()
        saveWatchProgress()
        val isPip = Build.VERSION.SDK_INT >= Build.VERSION_CODES.N && isInPictureInPictureMode
        if (!isPip) {
            playWhenReadyState = player?.playWhenReady ?: false
            player?.pause()
        }
    }

    override fun onResume() {
        super.onResume()
        hideSystemBars()
        if (playWhenReadyState && errorOverlay.visibility != View.VISIBLE && loadingOverlay.visibility != View.VISIBLE) {
            player?.play()
        }
    }

    override fun onStop() {
        super.onStop()
        saveWatchProgress()
        val isPip = Build.VERSION.SDK_INT >= Build.VERSION_CODES.N && isInPictureInPictureMode
        if (!isPip) {
            player?.pause()
        }
    }

    override fun onDestroy() {
        resolveJob?.cancel()
        seekFeedbackJob?.cancel()
        saveWatchProgress()
        val idToClose = activeStreamId
        activeStreamId = ""
        if (idToClose.isNotEmpty()) {
            closeStreamSessionAsync(idToClose)
        }
        playerView.player = null
        player?.release()
        player = null
        super.onDestroy()
    }

    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        val controller = WindowInsetsControllerCompat(window, window.decorView)
        controller.hide(WindowInsetsCompat.Type.systemBars())
        controller.systemBarsBehavior =
            WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    }
}
