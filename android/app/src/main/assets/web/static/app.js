/* AnimePahe GUI — frontend logic */

(() => {
  const $ = (id) => document.getElementById(id);
  const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
  const $$$ = (sel, root = document) => [...root.querySelectorAll(sel)];

  /* ─────────────────────────────────────────────────────────────────
   *  small utilities
   * ───────────────────────────────────────────────────────────────── */

  const fmtTime = (ts) => {
    const d = new Date(ts);
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  };
  // Keep concurrency within the range the native engine accepts.
  const clampFragments = (value, fallback = 8) => {
    const parsed = parseInt(value, 10);
    if (!Number.isFinite(parsed)) return fallback;
    return Math.min(64, Math.max(1, parsed));
  };
  const fmtBytes = (b) => {
    if (!Number.isFinite(b)) return "—";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0, v = b;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return i === 0 ? `${v} ${units[i]}` : `${v.toFixed(1)} ${units[i]}`;
  };
  const debounce = (fn, ms = 120) => {
    let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
  };

  const DQUOTE = String.fromCharCode(34);
  const AMP = String.fromCharCode(38);
  const escapeHtml = (s) => String(s).replace(/[&<>"']/g, (c) => {
    if (c === AMP) return AMP + "amp;";
    if (c === "<") return AMP + "lt;";
    if (c === ">") return AMP + "gt;";
    if (c === DQUOTE) return AMP + "quot;";
    return AMP + "#39;";
  });

  const token = () => document.querySelector('meta[name="app-token"]')?.content || "";

  // Older servers returned the cookie in the config payload. Normalise either
  // shape without keeping the secret in frontend state.
  const normalizeConfig = (config) => {
    const cf = config.cf || "";
    return {
      ua: config.ua || "",
      // The Rust API uses camelCase; older Python responses used snake_case.
      concurrent_fragments: config.concurrentFragments ?? config.concurrent_fragments ?? 8,
      downloadDir: config.downloadDir || config.download_dir || "",
      defaultDownloadDir:
        config.defaultDownloadDir || config.default_download_dir || config.downloadDir || config.download_dir || "",
      cfConfigured: config.cfConfigured ?? config.cf_configured ?? Boolean(cf),
      cfLength: config.cfLength ?? config.cf_length ?? cf.length,
    };
  };

  /* ─────────────────────────────────────────────────────────────────
   *  state
   * ───────────────────────────────────────────────────────────────── */

  const CATALOG_PAGE_SIZE = 40;
  const RECENT_CHUNK_SIZE = 30;
  // Download logs arrive much faster than a user can read them. Keep the DOM
  // bounded so a long batch cannot turn every SSE event into a layout pass.
  const MAX_LOG_LINES = 1000;
  const MAX_QUEUED_LOGS = MAX_LOG_LINES * 2;
  const emptyOptions = () => ({ resolutions: [], audios: [], combinations: [], sourceCount: 0, episode: "", session: "", filesizes: {} });

  let state = {
    localCatalog: [],
    remoteCatalog: [],
    catalogLimit: CATALOG_PAGE_SIZE,
    recentLimit: RECENT_CHUNK_SIZE,
    catalogObserver: null,
    searchMode: "local",
    selectedSlug: "",
    selectedTitle: "",
    episodes: [],
    epPage: 1,
    epLastPage: 1,
    epFilter: "",
    audioFilter: "",
    epTotal: 0,
    epTotalAll: 0,
    availableAudios: [],
    episodeSizes: {},
    episodesSelected: [],
    options: emptyOptions(),
    recentItems: [],
    recentPage: 1,
    recentLastPage: 1,
    recentTotal: 0,
    recentLoading: false,
    pendingSelectEpisode: null,
    allKnownEpisodes: {},
    config: { ua: "", concurrent_fragments: 8, cfConfigured: false, cfLength: 0 },
    deps: [],
    job: null,
    logSeq: 0,
    lastLogSeq: 0,
    logFilter: "all",
    autoscroll: true,
    activityVisible: window.innerWidth > 900,
    lastDoneJob: "",
    epStatusKey: "",
    playlistKey: "",
    // Navigation history for back button handling
    tabHistory: ["home"],
    activeTab: "home",
  };

  let logQueue = [];
  let logFrame = null;

  // Episode ids are always strings so that they match the DOM and the CLI spec.
  const epId = (value) => String(value);

  /* ─────────────────────────────────────────────────────────────────
   *  API helpers
   * ───────────────────────────────────────────────────────────────── */

  const parseError = async (res) => {
    const raw = await res.text();
    try {
      const data = JSON.parse(raw);
      if (data && data.error) return String(data.error);
    } catch (e) { /* not JSON */ }
    return raw || ("HTTP " + res.status);
  };

  const api = {
    get: async (path) => {
      const res = await fetch(path, { headers: { "X-App-Token": token() } });
      if (!res.ok) throw new Error(await parseError(res));
      return res.json();
    },
    post: async (path, body) => {
      const res = await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-App-Token": token() },
        body: JSON.stringify(body),
      });
      if (!res.ok) throw new Error(await parseError(res));
      return res.json();
    },
    events: null,
  };

  function initEventSource() {
    api.events = new EventSource(`/api/events?token=${encodeURIComponent(token())}`);
    // The server sends named `log` events; EventSource's onmessage only sees
    // unnamed events, which is why the activity panel used to stay empty.
    api.events.addEventListener("log", (e) => handleLog(JSON.parse(e.data)));
    api.events.onmessage = (e) => handleLog(JSON.parse(e.data));
    api.events.addEventListener("state", (e) => handleState(JSON.parse(e.data)));
    api.events.addEventListener("done", (e) => handleDone(JSON.parse(e.data)));
    api.events.onerror = () => { /* reconnect handled by browser */ };
  }

  /* Detect Cloudflare / cookie problems so the UI can point at the fix. */
  function noteApiError(message) {
    const text = String(message || "").toLowerCase();
    const broken =
      text.includes("cf_clearance") ||
      text.includes("cloudflare") ||
      text.includes("just a moment") ||
      text.includes("expired") ||
      text.includes("403 challenge") ||
      text.includes("403");
    if (!broken) return;
    const banner = $("cookieBanner");
    if (banner) {
      $("cookieBannerText").textContent = message;
      banner.classList.remove("hidden");
    }
    highlightSolveCfButton("needed");
  }

  /* ─────────────────────────────────────────────────────────────────
   *  Android storage permission check & banner
   * ───────────────────────────────────────────────────────────────── */
  function checkAndroidStoragePermission() {
    if (window.AndroidBridge && typeof window.AndroidBridge.hasStoragePermission === "function") {
      const hasPerm = window.AndroidBridge.hasStoragePermission();
      const banner = $("storagePermissionBanner");
      if (banner) {
        if (!hasPerm && !sessionStorage.getItem("storage_banner_dismissed")) {
          banner.classList.remove("hidden");
        } else {
          banner.classList.add("hidden");
        }
      }
    }
  }

  window.onStoragePermissionChanged = function(hasPerm) {
    const banner = $("storagePermissionBanner");
    if (banner) {
      if (hasPerm) {
        banner.classList.add("hidden");
        sessionStorage.removeItem("storage_banner_dismissed");
      } else if (!sessionStorage.getItem("storage_banner_dismissed")) {
        banner.classList.remove("hidden");
      }
    }
  };

  // Test connection to animepahe.pw
  async function testConnection() {
    try {
      const res = await api.post("/api/test-connection", {});
      return res.ok === true;
    } catch (e) {
      return false;
    }
  }

  // Check connection and update UI accordingly
  async function checkConnectionAndUpdateUI() {
    const connected = await testConnection();
    const solveBtn = $("topbarSolveCf");
    const bannerBtn = $("bannerSolveCookie");
    const statusPill = $("statusPill");
    const statusText = $("statusText");

    if (connected) {
      // Connection OK - show subtle indicator
      if (statusPill && statusText) {
        statusPill.dataset.state = "connected";
        statusText.textContent = "Connected";
        // Reset after 3 seconds to idle
        setTimeout(() => {
          if (statusPill.dataset.state === "connected") {
            statusPill.dataset.state = "idle";
            statusText.textContent = "Idle";
          }
        }, 3000);
      }
      // Show green "solved" state on buttons
      highlightSolveCfButton("solved");
      // Fade to normal after a moment
      setTimeout(() => highlightSolveCfButton(false), 4000);
    } else {
      // Connection failed - highlight the Solve Cloudflare button (red)
      highlightSolveCfButton("needed");
      if (statusPill && statusText) {
        statusPill.dataset.state = "disconnected";
        statusText.textContent = "Cloudflare blocked";
      }
    }
    return connected;
  }

  // Highlight or un-highlight the Solve Cloudflare buttons
  // State: "needed" / true = red pulsing, "solved" = green pulsing, false/undefined = normal
  function highlightSolveCfButton(state) {
    const solveBtn = $("topbarSolveCf");
    const bannerBtn = $("bannerSolveCookie");
    
    // Remove both classes first
    if (solveBtn) {
      solveBtn.classList.remove("cf-needed", "cf-solved");
    }
    if (bannerBtn) {
      bannerBtn.classList.remove("cf-needed", "cf-solved");
    }
    
    if (state === "needed" || state === true) {
      if (solveBtn) solveBtn.classList.add("cf-needed");
      if (bannerBtn) bannerBtn.classList.add("cf-needed");
    } else if (state === "solved") {
      if (solveBtn) solveBtn.classList.add("cf-solved");
      if (bannerBtn) bannerBtn.classList.add("cf-solved");
    }
  }

  /* ─────────────────────────────────────────────────────────────────
   *  In-App Auto Update
   * ───────────────────────────────────────────────────────────────── */
  let latestUpdateInfo = null;

  async function checkAppUpdates(silent = true) {
    const statusEl = $("updateCheckStatus");
    const checkBtn = $("btnCheckUpdates");
    if (statusEl && !silent) statusEl.textContent = "Checking GitHub…";
    if (checkBtn) setBusy(checkBtn, true);

    try {
      const res = await api.get("/api/check-update");
      if (!res.ok) {
        if (!silent) {
          toast(res.error || "Could not check updates", "warn");
          if (statusEl) statusEl.textContent = "Check failed";
        }
        return;
      }

      if (res.current_version && $("appVersionBadge")) {
        $("appVersionBadge").textContent = "v" + res.current_version;
      }

      latestUpdateInfo = res;

      if (res.update_available) {
        showUpdateBanner(res);
        if (!silent) {
          openUpdateModal(res);
          if (statusEl) statusEl.textContent = `Update available: v${res.latest_version}`;
        }
      } else {
        hideUpdateBanner();
        if (!silent) {
          toast(`✓ PaheFlow is up to date (v${res.current_version})`, "ok");
          if (statusEl) statusEl.textContent = "Up to date";
        }
      }
    } catch (e) {
      if (!silent) {
        toast("Check failed: " + e.message, "error");
        if (statusEl) statusEl.textContent = "Error checking updates";
      }
    } finally {
      if (checkBtn) setBusy(checkBtn, false);
    }
  }

  function showUpdateBanner(info) {
    const banner = $("updateBanner");
    if (!banner) return;
    if (sessionStorage.getItem("paheflow_update_dismissed_" + info.latest_version)) return;
    const title = $("updateBannerHeading");
    const text = $("updateBannerText");
    if (title) title.textContent = `Update Available (v${info.latest_version})!`;
    if (text) text.textContent = info.title || "A newer version of PaheFlow is ready to install.";
    banner.classList.remove("hidden");
  }

  function hideUpdateBanner() {
    const banner = $("updateBanner");
    if (banner) banner.classList.add("hidden");
  }

  function openUpdateModal(info) {
    if (!info) info = latestUpdateInfo;
    if (!info) return;
    const modal = $("updateModal");
    if (!modal) return;
    const ver = $("updateModalVersion");
    const notes = $("updateModalNotes");
    const progressCont = $("updateDownloadProgressContainer");
    if (ver) ver.textContent = `v${info.current_version} → v${info.latest_version}`;
    if (notes) notes.textContent = info.notes || "No release notes provided.";
    if (progressCont) progressCont.classList.add("hidden");
    const actionBtn = $("confirmUpdateBtn");
    if (actionBtn) {
      actionBtn.disabled = false;
      actionBtn.textContent = "⬇️ Download & Install";
    }
    modal.classList.remove("hidden");
  }

  function closeUpdateModal() {
    const modal = $("updateModal");
    if (modal) modal.classList.add("hidden");
  }

  function performAppUpdate() {
    if (!latestUpdateInfo) return;
    const actionBtn = $("confirmUpdateBtn");
    const progressCont = $("updateDownloadProgressContainer");
    const progressFill = $("updateProgressFill");
    const progressPercent = $("updateProgressPercent");
    const progressLabel = $("updateProgressLabel");

    if (window.AndroidBridge && typeof window.AndroidBridge.downloadLatestUpdate === "function") {
      if (actionBtn) {
        actionBtn.disabled = true;
        actionBtn.textContent = "Downloading…";
      }
      if (progressCont) progressCont.classList.remove("hidden");
      if (progressFill) progressFill.style.width = "0%";
      if (progressPercent) progressPercent.textContent = "0%";
      if (progressLabel) progressLabel.textContent = "Starting download…";

      const apkUrl = latestUpdateInfo.apk_url || latestUpdateInfo.html_url;
      window.AndroidBridge.downloadLatestUpdate(apkUrl);
    } else {
      // Desktop / Browser
      const targetUrl = latestUpdateInfo.html_url || "https://github.com/sumon317/paheflow/releases/latest";
      window.open(targetUrl, "_blank");
      closeUpdateModal();
      toast("Opened GitHub release in browser", "info");
    }
  }

  window.onUpdateDownloadProgress = function(percent, statusText) {
    const progressCont = $("updateDownloadProgressContainer");
    const progressFill = $("updateProgressFill");
    const progressPercent = $("updateProgressPercent");
    const progressLabel = $("updateProgressLabel");
    if (progressCont) progressCont.classList.remove("hidden");
    if (progressFill) progressFill.style.width = `${percent}%`;
    if (progressPercent) progressPercent.textContent = `${percent}%`;
    if (progressLabel && statusText) progressLabel.textContent = statusText;
  };

  window.onUpdateDownloadFailed = function(errMsg) {
    const actionBtn = $("confirmUpdateBtn");
    if (actionBtn) {
      actionBtn.disabled = false;
      actionBtn.textContent = "Retry Download";
    }
    toast("Update download failed: " + errMsg, "error");
  };

  window.onUpdateDownloadComplete = function() {
    const actionBtn = $("confirmUpdateBtn");
    if (actionBtn) {
      actionBtn.disabled = true;
      actionBtn.textContent = "Downloaded — open the popup or check Downloads/PaheFlow";
    }
    const progressLabel = $("updateProgressLabel");
    if (progressLabel) progressLabel.textContent = "Saved to Downloads/PaheFlow";
  };

  /* ─────────────────────────────────────────────────────────────────
   *  toasts
   * ───────────────────────────────────────────────────────────────── */

  function toast(msg, type = "info") {
    const el = document.createElement("div");
    el.className = `toast ${type === "error" ? "error" : type === "warn" ? "warn" : type === "ok" ? "success" : ""}`;
    el.textContent = msg;
    $("toasts").appendChild(el);
    setTimeout(() => { el.style.animation = "slideIn .22s ease reverse"; setTimeout(() => el.remove(), 220); }, 3500);
  }

  /* ─────────────────────────────────────────────────────────────────
   *  render: catalog / search
   * ───────────────────────────────────────────────────────────────── */

  function filteredCatalog() {
    if (state.searchMode !== "local") return state.remoteCatalog;
    const term = $("searchInput").value.trim().toLowerCase();
    if (!term) return state.localCatalog;
    return state.localCatalog.filter(a =>
      a.title.toLowerCase().includes(term) || a.slug.toLowerCase().includes(term));
  }

  const CATALOG_CHUNK_SIZE = 40;
  const RECENT_CHUNK_SIZE = 30;

  function renderCatalog(resetLimit = true) {
    if (state.searchMode === "recent") {
      renderRecentReleases(resetLimit);
      return;
    }
    const list = filteredCatalog();
    const box = $("resultsList");
    if (!list.length) {
      if (state.catalogObserver) {
        state.catalogObserver.disconnect();
        state.catalogObserver = null;
      }
      const remote = state.searchMode === "remote";
      const hasQuery = Boolean($("searchInput").value.trim());
      box.innerHTML = `<div class="empty"><p class="empty-title">${
        remote
          ? (hasQuery ? "No results" : "Search animepahe.pw")
          : "No matches in local catalog"
      }</p><p class="empty-text">${
        remote
          ? (hasQuery ? "Try a different search term." : "Type an anime title in the search box above and press Search.")
          : "The local list holds 6,600+ cached slugs; try another search or check recent releases."
      }</p></div>`;
      updateCatalogMeta(0, 0);
      return;
    }

    if (resetLimit) {
      state.catalogLimit = CATALOG_CHUNK_SIZE;
    }

    const total = list.length;
    const shownCount = Math.min(state.catalogLimit, total);
    updateCatalogMeta(shownCount, total);

    const shown = list.slice(0, shownCount);
    const isLocal = state.searchMode === "local";

    const cardsHtml = shown.map(a => renderCatalogCardHtml(a, isLocal)).join("");
    const hasMore = shownCount < total;
    const sentinelHtml = hasMore ? `<div id="catalogSentinel" class="catalog-sentinel" style="padding: 16px; text-align: center; color: var(--text-faint); font-size: 12px; display: flex; align-items: center; justify-content: center; gap: 8px;"><span class="spinner" style="width: 14px; height: 14px;"></span> Loading more (${shownCount} of ${total})...</div>` : "";

    box.innerHTML = cardsHtml + sentinelHtml;
    setupCatalogObserver(list, isLocal);
  }

  function renderCatalogCardHtml(a, isLocal) {
    const poster = isLocal ? "" : (a.poster || a.cover || "");
    const thumbUrl = poster ? `/api/thumb?url=${encodeURIComponent(poster)}&token=${encodeURIComponent(token())}` : "";
    const isSelected = a.slug === state.selectedSlug;

    return `
      <div class="result ${isSelected ? "is-selected" : ""}" data-slug="${escapeHtml(a.slug)}">
        ${poster ? `
          <div class="recent-thumb-wrap">
            <img class="recent-thumb" src="${escapeHtml(thumbUrl)}" alt="${escapeHtml(a.title)}" loading="lazy" decoding="async" onerror="this.parentElement.style.display='none'" />
          </div>
        ` : ""}
        <div class="result-main">
          <div class="result-title">${escapeHtml(a.title)}</div>
          <div class="result-meta">
            <span class="slug">${escapeHtml(a.slug)}</span>
          </div>
        </div>
        <div class="result-actions">
          <button class="btn btn-ghost btn-sm select-btn" title="Select this anime">Select</button>
        </div>
      </div>
    `;
  }

  function setupCatalogObserver(list, isLocal) {
    if (state.catalogObserver) {
      state.catalogObserver.disconnect();
      state.catalogObserver = null;
    }
    const sentinel = $("catalogSentinel");
    if (!sentinel) return;

    state.catalogObserver = new IntersectionObserver((entries) => {
      if (entries[0] && entries[0].isIntersecting) {
        appendMoreCatalog(list, isLocal);
      }
    }, { rootMargin: "600px" });
    state.catalogObserver.observe(sentinel);
  }

  function appendMoreCatalog(list, isLocal) {
    const total = list.length;
    if (state.catalogLimit >= total) return;

    const start = state.catalogLimit;
    state.catalogLimit = Math.min(state.catalogLimit + CATALOG_CHUNK_SIZE, total);
    updateCatalogMeta(state.catalogLimit, total);

    const nextBatch = list.slice(start, state.catalogLimit);
    const box = $("resultsList");
    const sentinel = $("catalogSentinel");

    const fragment = document.createDocumentFragment();
    const temp = document.createElement("div");
    temp.innerHTML = nextBatch.map(a => renderCatalogCardHtml(a, isLocal)).join("");
    while (temp.firstChild) {
      fragment.appendChild(temp.firstChild);
    }

    if (sentinel) {
      box.insertBefore(fragment, sentinel);
      if (state.catalogLimit >= total) {
        sentinel.remove();
        if (state.catalogObserver) {
          state.catalogObserver.disconnect();
          state.catalogObserver = null;
        }
      } else {
        sentinel.innerHTML = `<span class="spinner" style="width: 14px; height: 14px;"></span> Loading more (${state.catalogLimit} of ${total})...`;
      }
    } else {
      box.appendChild(fragment);
    }
  }

  function updateCatalogMeta(shown, total) {
    if (state.searchMode === "recent") {
      const totalStr = state.recentTotal ? ` of ${state.recentTotal.toLocaleString()}` : "";
      $("catalogMeta").textContent = `${shown} recent release${shown !== 1 ? "s" : ""}${totalStr ? ` (${totalStr.trim()} total)` : ""} from AnimePahe`;
    } else if (state.searchMode === "remote") {
      const hasQuery = Boolean($("searchInput").value.trim());
      $("catalogMeta").textContent = total
        ? `${total} result(s) from animepahe.pw`
        : "No results from animepahe.pw";
    } else {
      $("catalogMeta").textContent = total
        ? `${total} cached slug(s) shown`
        : "No cached slugs match";
    }
  }

  function setCatalogMode(mode, forceReload = false) {
    if (state.catalogObserver) {
      state.catalogObserver.disconnect();
      state.catalogObserver = null;
    }
    const isModeChange = state.searchMode !== mode;
    state.searchMode = mode;
    state.catalogLimit = CATALOG_CHUNK_SIZE;
    state.recentLimit = RECENT_CHUNK_SIZE;
    $$("#resultModes .seg").forEach(b => b.classList.toggle("is-active", b.dataset.mode === mode));
    const pager = $("recentPager");
    if (pager) {
      pager.classList.add("hidden");
    }
    if (mode === "recent") {
      if (!state.recentItems.length || !isModeChange || forceReload) {
        loadRecentReleases(1, true);
      } else {
        renderRecentReleases(true);
      }
    } else {
      renderCatalog(true);
    }
  }

  function timeAgo(dateStr) {
    if (!dateStr) return "";
    try {
      const d = new Date(dateStr.replace(" ", "T") + "Z");
      const diff = Math.floor((Date.now() - d.getTime()) / 1000);
      if (isNaN(diff) || diff < 0) return dateStr;
      if (diff < 60) return "Just now";
      if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
      if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
      return `${Math.floor(diff / 86400)}d ago`;
    } catch (_) {
      return dateStr;
    }
  }

  async function loadRecentReleases(page = 1, all = false) {
    state.recentLoading = true;
    state.recentPage = page;
    const box = $("resultsList");
    box.innerHTML = `<div class="ep-loading"><span class="spinner"></span> Loading recent releases from AnimePahe...</div>`;
    $("catalogMeta").textContent = "Fetching latest releases...";
    updateRecentPager();
    try {
      const res = await api.get(`/api/latest?page=${page}&all=${all ? "1" : "0"}`);
      const list = res.items || res.data || [];
      state.recentItems = list;
      state.recentPage = 1;
      state.recentLastPage = 1;
      state.recentTotal = res.total ?? list.length;
      state.recentLoading = false;
      if (state.searchMode === "recent") {
        renderRecentReleases(true);
      }
    } catch (e) {
      state.recentLoading = false;
      if (state.searchMode === "recent") {
        box.innerHTML = `<div class="empty"><p class="empty-title">Failed to load recent releases</p><p class="empty-text">${escapeHtml(e.message || "Network or Cloudflare error")}</p></div>`;
        $("catalogMeta").textContent = "Failed to load latest releases";
        updateRecentPager();
      }
      toast("Recent releases failed: " + e.message, "error");
      noteApiError(e.message);
    }
  }

  function renderRecentReleases(resetLimit = true) {
    const box = $("resultsList");
    const items = state.recentItems || [];
    if (!items.length) {
      if (state.catalogObserver) {
        state.catalogObserver.disconnect();
        state.catalogObserver = null;
      }
      box.innerHTML = `<div class="empty"><p class="empty-title">No recent releases found</p><p class="empty-text">Check your internet connection or Cloudflare clearance.</p></div>`;
      $("catalogMeta").textContent = "No recent releases";
      updateRecentPager();
      return;
    }

    if (resetLimit) {
      state.recentLimit = RECENT_CHUNK_SIZE;
    }

    const total = items.length;
    const shownCount = Math.min(state.recentLimit || RECENT_CHUNK_SIZE, total);
    const shown = items.slice(0, shownCount);

    const cardsHtml = shown.map(item => renderRecentCardHtml(item)).join("");
    const hasMore = shownCount < total;
    const sentinelHtml = hasMore ? `<div id="recentSentinel" class="recent-sentinel" style="padding: 16px; text-align: center; color: var(--text-faint); font-size: 12px; display: flex; align-items: center; justify-content: center; gap: 8px;"><span class="spinner" style="width: 14px; height: 14px;"></span> Loading more releases (${shownCount} of ${total})...</div>` : "";

    box.innerHTML = cardsHtml + sentinelHtml;

    const totalStr = state.recentTotal ? ` of ${state.recentTotal.toLocaleString()}` : "";
    $("catalogMeta").textContent = `${shownCount}${totalStr} latest releases from AnimePahe`;
    updateRecentPager();

    setupRecentObserver(items);
  }

  function renderRecentCardHtml(item) {
    const title = item.anime_title || item.animeTitle || "";
    const session = item.anime_session || item.animeSession || "";
    const epNum = (item.episode !== undefined && item.episode !== null) ? String(item.episode) : "";
    const isDub = (item.audio || "").toLowerCase() === "dub" || (item.audio || "").toLowerCase() === "eng";
    const audioClass = isDub ? "recent-badge-dub" : "recent-badge-sub";
    const audioLabel = isDub ? "DUB" : "SUB";
    const timeStr = timeAgo(item.created_at || item.createdAt);
    const isSelected = session === state.selectedSlug;
    const snapshot = item.snapshot || "";
    const thumbUrl = snapshot ? `/api/thumb?url=${encodeURIComponent(snapshot)}&token=${encodeURIComponent(token())}` : "";
    const duration = item.duration || "";

    return `
      <div class="result recent ${isSelected ? "is-selected" : ""}" data-session="${escapeHtml(session)}" data-title="${escapeHtml(title)}" data-ep="${escapeHtml(epNum)}">
        ${snapshot ? `
          <div class="recent-thumb-wrap">
            <img class="recent-thumb" src="${escapeHtml(thumbUrl)}" alt="${escapeHtml(title)}" loading="lazy" decoding="async" onerror="this.parentElement.style.display='none'" />
          </div>
        ` : ""}
        <div class="result-main">
          <div class="result-title">${escapeHtml(title)}</div>
          <div class="result-meta">
            <span class="badge recent-badge-ep">EP ${escapeHtml(epNum)}</span>
            <span class="badge ${audioClass}">${audioLabel}</span>
            ${duration ? `<span>${escapeHtml(duration)}</span>` : ""}
            ${timeStr ? `<span class="recent-time">🕒 ${escapeHtml(timeStr)}</span>` : ""}
          </div>
        </div>
        <div class="result-actions">
          <button class="btn btn-ghost btn-sm select-btn" title="Open Episode ${escapeHtml(epNum)}">Open</button>
        </div>
      </div>
    `;
  }

  function setupRecentObserver(items) {
    if (state.catalogObserver) {
      state.catalogObserver.disconnect();
      state.catalogObserver = null;
    }
    const sentinel = $("recentSentinel");
    if (!sentinel) return;

    state.catalogObserver = new IntersectionObserver((entries) => {
      if (entries[0] && entries[0].isIntersecting) {
        appendMoreRecent(items);
      }
    }, { rootMargin: "600px" });
    state.catalogObserver.observe(sentinel);
  }

  function appendMoreRecent(items) {
    const total = items.length;
    if (state.recentLimit >= total) return;

    const start = state.recentLimit;
    state.recentLimit = Math.min(state.recentLimit + RECENT_CHUNK_SIZE, total);

    const totalStr = state.recentTotal ? ` of ${state.recentTotal.toLocaleString()}` : "";
    $("catalogMeta").textContent = `${state.recentLimit}${totalStr} latest releases from AnimePahe`;

    const nextBatch = items.slice(start, state.recentLimit);
    const box = $("resultsList");
    const sentinel = $("recentSentinel");

    const fragment = document.createDocumentFragment();
    const temp = document.createElement("div");
    temp.innerHTML = nextBatch.map(item => renderRecentCardHtml(item)).join("");
    while (temp.firstChild) {
      fragment.appendChild(temp.firstChild);
    }

    if (sentinel) {
      box.insertBefore(fragment, sentinel);
      if (state.recentLimit >= total) {
        sentinel.remove();
        if (state.catalogObserver) {
          state.catalogObserver.disconnect();
          state.catalogObserver = null;
        }
      } else {
        sentinel.innerHTML = `<span class="spinner" style="width: 14px; height: 14px;"></span> Loading more releases (${state.recentLimit} of ${total})...`;
      }
    } else {
      box.appendChild(fragment);
    }
  }

  function updateRecentPager() {
    const pager = $("recentPager");
    if (!pager) return;
    pager.classList.add("hidden");
  }

  function selectRecentEpisode(slug, title, episodeNum) {
    if (!slug) return;
    state.selectedTitle = title || slug;
    state.selectedSlug = slug;
    state.pendingSelectEpisode = String(episodeNum);
    $$(".result").forEach(el => el.classList.toggle("is-selected", el.dataset.session === slug || el.dataset.slug === slug));
    showTab("episodes");
    loadEpisodes(slug);
  }

  async function doSearch() {
    const q = $("searchInput").value.trim();
    if (!q) { setCatalogMode("local"); return; }
    setCatalogMode("remote");
    setBusy($("searchBtn"), true);
    try {
      const { items } = await api.get(`/api/search?q=${encodeURIComponent(q)}`);
      state.remoteCatalog = items;
      renderCatalog();
      $("cookieBanner").classList.add("hidden");
    } catch (e) {
      toast("Search failed: " + e.message, "error");
      noteApiError(e.message);
    }
    finally { setBusy($("searchBtn"), false); }
  }

  async function refreshCatalog() {
    if (state.searchMode === "recent") {
      setBusy($("refreshCatalog"), true);
      try {
        await loadRecentReleases(state.recentPage || 1, true);
        toast("Recent releases refreshed", "ok");
      } catch (e) {
        toast("Refresh failed: " + e.message, "error");
        noteApiError(e.message);
      } finally {
        setBusy($("refreshCatalog"), false);
      }
      return;
    }
    if (state.searchMode === "remote") {
      const q = $("searchInput").value.trim();
      if (q) {
        await doSearch();
      } else {
        setCatalogMode("local");
      }
      return;
    }
    setBusy($("refreshCatalog"), true);
    try {
      const { items } = await api.get("/api/catalog?refresh=1");
      state.localCatalog = items;
      setCatalogMode("local");
      toast("Catalog refreshed (" + items.length + " entries)", "ok");
    } catch (e) {
      toast("Refresh failed: " + e.message, "error");
      noteApiError(e.message);
    }
    finally { setBusy($("refreshCatalog"), false); }
  }

  function selectAnime(slug) {
    const entry = [...state.localCatalog, ...state.remoteCatalog].find(a => a.slug === slug);
    if (!entry) return;
    state.selectedTitle = entry.title;
    $$(".result").forEach(el => el.classList.toggle("is-selected", el.dataset.slug === slug));
    showTab("episodes");
    loadEpisodes(slug);
  }

  /* ─────────────────────────────────────────────────────────────────
   *  render: episodes
   * ───────────────────────────────────────────────────────────────── */

  async function loadEpisodes(slug, page = 1, audio = null) {
    if (!slug) return;
    const animeChanged = state.selectedSlug !== slug;
    state.selectedSlug = slug;
    state.epPage = page;
    if (audio !== null) {
      state.audioFilter = audio;
    }
    state.episodes = [];
    state.epLastPage = 1;
    if (animeChanged) {
      // Episode numbers are only meaningful inside one anime. Keeping the
      // previous selection could silently download episodes of the new show.
      state.epNotFound = false;
      state.episodesSelected = [];
      state.allKnownEpisodes = {};
      state.epFilter = "";
      $("epFilter").value = "";
      if (audio === null) {
        state.audioFilter = "";
      }
      state.availableAudios = [];
      state.episodeSizes = {};
      state.options = emptyOptions();
      fillOptionSelects();
    }
    syncAudioControlUI();
    renderEpGrid();
    updatePagination();
    updateCommandPreview();
    setBusy($("reloadEpisodes"), true);
    try {
      const titleParam = state.selectedTitle ? `&title=${encodeURIComponent(state.selectedTitle)}` : "";
      const audioParam = state.audioFilter ? `&audio=${encodeURIComponent(state.audioFilter)}` : "";
      const res = await api.get(`/api/episodes?slug=${encodeURIComponent(slug)}&all=1${titleParam}${audioParam}`);
      if (res.slug && res.slug !== slug) {
        state.selectedSlug = res.slug;
        slug = res.slug;
      }
      if (state.selectedSlug !== slug || state.epPage !== page) return;
      state.epNotFound = !!res.notFound;
      state.episodes = res.items || [];
      state.epLastPage = res.lastPage ?? 1;
      state.epPage = res.page ?? page;
      state.epTotal = res.total ?? state.episodes.length;
      state.epTotalAll = res.totalAll ?? state.epTotal;
      state.availableAudios = res.availableAudios || [];

      (res.items || []).forEach(e => {
        state.allKnownEpisodes[epId(e.episode)] = e;
      });

      const displayList = getDisplayEpisodes();
      if (state.pendingSelectEpisode) {
        const targetEp = state.pendingSelectEpisode;
        const exists = displayList.some(e => epId(e.episode) === targetEp);
        if (exists) {
          state.episodesSelected = [targetEp];
        }
        state.pendingSelectEpisode = null;
      } else {
        // Prune any selected episodes not present in the filtered set
        const validEpIds = new Set(displayList.map(e => epId(e.episode)));
        state.episodesSelected = state.episodesSelected.filter(id => validEpIds.has(id));
      }

      syncAudioControlUI();
      renderEpGrid();
      updatePagination();
      if (!res.notFound) {
        probeFirstEpisode();
      }
      $("cookieBanner").classList.add("hidden");
      if (res.stale) {
        // The site refused the live request; this list came from the disk cache or not found.
        if (res.warning && !res.notFound) {
          toast(res.warning, "warn");
        }
      }
    } catch (e) {
      const is404 = String(e.message || "").includes("404") || String(e.message || "").toLowerCase().includes("not found");
      if (is404) {
        state.epNotFound = true;
        state.episodes = [];
        renderEpGrid();
        updatePagination();
      } else {
        toast("Failed to load episodes: " + e.message, "error");
        noteApiError(e.message);
      }
    }
    finally { setBusy($("reloadEpisodes"), false); }
  }

  function syncAudioControlUI() {
    const norm = (state.audioFilter || "").toLowerCase();
    const segs = $$("#epAudioSegmented .seg");
    segs.forEach(b => {
      const bAud = (b.dataset.audio || "").toLowerCase();
      const isActive = (norm === "" && bAud === "")
        || ((norm === "dub" || norm === "eng") && bAud === "dub")
        || ((norm === "sub" || norm === "jpn") && bAud === "sub");
      b.classList.toggle("is-active", isActive);
    });

    const optAudio = $("optAudio");
    if (optAudio) {
      if (norm === "dub" || norm === "eng") {
        optAudio.value = "eng";
      } else if (norm === "sub" || norm === "jpn") {
        optAudio.value = "jpn";
      } else {
        optAudio.value = "";
      }
    }

    const btnDub = $("btnAudioDub");
    if (btnDub) {
      if (state.availableAudios.length && !state.availableAudios.includes("eng")) {
        btnDub.title = "No English Dub episodes released for this anime yet";
      } else {
        btnDub.title = "Filter to English Dub episodes";
      }
    }
  }

  function updatePagination() {
    const pager = $("epPager");
    if (!pager) return;
    pager.classList.add("hidden"); // Always hide - we load all episodes at once
  }

  function getDisplayEpisodes() {
    const list = state.episodes || [];
    const norm = (state.audioFilter || "").toLowerCase();
    if (norm === "dub" || norm === "eng") {
      return list.filter(e => {
        const a = (e.audio || "").toLowerCase();
        return a === "eng" || a === "dub";
      });
    }
    return list;
  }

  function renderEpGrid() {
    const box = $("epGrid");
    const displayList = getDisplayEpisodes();
    const filtered = displayList.filter(e => {
      const hay = ("E" + epId(e.episode) + " " + epId(e.episode) + " " + (e.audio || "") + " " + (e.created_at || "")).toLowerCase();
      return hay.includes(state.epFilter.toLowerCase());
    });
    if (!filtered.length) {
      let title = "No episodes match";
      let text = "Try adjusting your search filter.";
      if (state.epNotFound) {
        title = "No episodes found";
        text = "This title appears to have no releases or was removed on AnimePahe.";
      } else if (state.audioFilter === "dub" || state.audioFilter === "eng") {
        title = "No Dub episodes available";
        text = "English Dub episodes have not been released yet for this anime. Try switching to Sub or All.";
      } else if (state.audioFilter === "sub" || state.audioFilter === "jpn") {
        title = "No Sub episodes available";
        text = "No Japanese Sub episodes are available. Try switching to All.";
      }
      box.innerHTML = `<div class="empty"><p class="empty-title">${escapeHtml(title)}</p><p class="empty-text">${escapeHtml(text)}</p></div>`;
      updateEpSelectionUI();
      return;
    }
    function formatEpDur(dur) {
      if (!dur) return "";
      const parts = String(dur).trim().split(":");
      if (parts.length === 3) {
        const mins = parseInt(parts[1], 10);
        return mins ? `${mins}m` : "";
      }
      return "";
    }

    const norm = (state.audioFilter || "").toLowerCase();
    box.innerHTML = filtered.map((e) => {
      const id = epId(e.episode);
      const checked = state.episodesSelected.includes(id);
      const isEng = (e.audio || "").toLowerCase() === "eng" || (e.audio || "").toLowerCase() === "dub";
      let audioTag;
      if (norm === "dub" || norm === "eng") {
        audioTag = "DUB";
      } else if (norm === "sub" || norm === "jpn") {
        audioTag = isEng ? "SUB (DUB)" : "SUB";
      } else {
        audioTag = isEng ? "DUB" : (e.audio === "jpn" ? "SUB" : (e.audio || "—").toUpperCase());
      }
      const dur = formatEpDur(e.duration);
      const sizeTag = getEpisodeSizeBadge(e);
      const isDubTag = audioTag.includes("DUB");
      return `
      <div class="ep ${checked ? "is-checked" : ""}" data-ep="${escapeHtml(id)}" title="Episode ${escapeHtml(id)}${dur ? ` · ${dur}` : ""}${sizeTag ? ` · ${sizeTag}` : ""}">
        <input type="checkbox" ${checked ? "checked" : ""} style="pointer-events: none;" />
        <span class="ep-num">E${escapeHtml(id)}</span>
        <div class="ep-info">
          <div class="ep-tags">
            <span class="ep-audio-tag ${isDubTag ? "is-dub" : "is-sub"}">${escapeHtml(audioTag)}</span>
            ${sizeTag ? `<span class="ep-size-tag">${escapeHtml(sizeTag)}</span>` : ""}
          </div>
          ${dur ? `<span class="ep-dur">${dur}</span>` : ""}
        </div>
      </div>`;
    }).join("");
    updateEpSelectionUI();
  }

  function parseDurSecs(dur) {
    if (!dur) return 0;
    const parts = String(dur).trim().split(":");
    if (parts.length === 3) {
      return (parseInt(parts[0], 10) || 0) * 3600 + (parseInt(parts[1], 10) || 0) * 60 + (parseInt(parts[2], 10) || 0);
    }
    return 0;
  }

  function parseSizeToBytes(str) {
    if (!str) return 0;
    const m = String(str).match(/([\d\.]+)\s*(MB|GB|KB)/i);
    if (!m) return 0;
    const val = parseFloat(m[1]);
    const unit = m[2].toUpperCase();
    if (unit === "GB") return Math.round(val * 1024 * 1024 * 1024);
    if (unit === "MB") return Math.round(val * 1024 * 1024);
    if (unit === "KB") return Math.round(val * 1024);
    return 0;
  }

  function formatBytes(bytes) {
    if (!bytes || isNaN(bytes)) return "";
    const mb = bytes / (1024 * 1024);
    if (mb >= 1000) {
      return `${(mb / 1024).toFixed(1)} GB`;
    }
    return `${Math.round(mb)} MB`;
  }

  const RES_WEIGHTS = {
    "1080": 1.0,
    "720": 0.58,
    "480": 0.32,
    "360": 0.20,
  };

  function estimateSizeForResolution(baseBytes, baseRes, targetRes) {
    if (!baseBytes) return 0;
    if (!baseRes || !targetRes || baseRes === targetRes) return baseBytes;
    const wFrom = RES_WEIGHTS[baseRes] || (parseInt(baseRes, 10) / 1080) || 0.58;
    const wTo = RES_WEIGHTS[targetRes] || (parseInt(targetRes, 10) / 1080) || 0.58;
    if (!wFrom || !wTo) return baseBytes;
    return Math.round(baseBytes * (wTo / wFrom));
  }

  function getActiveResolution() {
    const chosen = $("optResolution")?.value;
    if (chosen) return chosen;
    if (state.options?.filesizes) {
      for (const r of (state.options?.resolutions || [])) {
        if (state.options.filesizes[r]) return r;
      }
    }
    return state.options?.resolutions?.[0] || "720";
  }

  function getEpisodeSizeBadge(e, res) {
    if (!e) return "";
    const id = epId(e.episode);
    const targetRes = res || getActiveResolution();

    // 1. Direct probed size for this episode and resolution
    const probed = state.episodeSizes?.[id]?.[targetRes] ||
      (state.options?.episode && epId(state.options.episode) === id ? state.options?.filesizes?.[targetRes] : null);
    if (probed) return probed;

    // 2. Find any base episode that has size in targetRes
    const baseEp = state.episodes.find(ep => {
      const ep_id = epId(ep.episode);
      return state.episodeSizes?.[ep_id]?.[targetRes] ||
        (state.options?.episode && epId(state.options.episode) === ep_id && state.options?.filesizes?.[targetRes]);
    }) || (state.options?.filesizes?.[targetRes] ? state.episodes[0] : null);

    if (baseEp) {
      const baseSize = state.episodeSizes?.[epId(baseEp.episode)]?.[targetRes] || state.options?.filesizes?.[targetRes];
      const baseBytes = parseSizeToBytes(baseSize);
      if (baseBytes) {
        const baseDurSecs = parseDurSecs(baseEp.duration);
        const epDurSecs = parseDurSecs(e.duration);
        if (baseDurSecs > 0 && epDurSecs > 0) {
          const estimated = Math.round(baseBytes * (epDurSecs / baseDurSecs));
          return `~${formatBytes(estimated)}`;
        }
        return `~${baseSize}`;
      }
    }

    // 3. Cross-resolution estimation: if targetRes has no probed size, but another resolution does
    if (state.options?.filesizes) {
      for (const [r, sz] of Object.entries(state.options.filesizes)) {
        if (sz && r !== targetRes) {
          const rawBytes = parseSizeToBytes(sz);
          if (rawBytes) {
            const scaledBytes = estimateSizeForResolution(rawBytes, r, targetRes);
            const ep0 = state.episodes[0] || Object.values(state.allKnownEpisodes)[0];
            const baseDurSecs = parseDurSecs(ep0?.duration);
            const epDurSecs = parseDurSecs(e.duration);
            if (baseDurSecs > 0 && epDurSecs > 0) {
              const estimated = Math.round(scaledBytes * (epDurSecs / baseDurSecs));
              return `~${formatBytes(estimated)}`;
            }
            return `~${formatBytes(scaledBytes)}`;
          }
        }
      }
    }

    return "";
  }

  function calculateTotalSelectedSize() {
    if (!state.episodesSelected.length) return "";
    const activeRes = getActiveResolution();

    // Find base size and duration from any known episode or options.filesizes
    let baseBytes = 0;
    let baseDurSecs = 0;

    // A. Check if any known episode has probed size for activeRes
    for (const ep of Object.values(state.allKnownEpisodes)) {
      const ep_id = epId(ep.episode);
      const sz = state.episodeSizes?.[ep_id]?.[activeRes];
      if (sz) {
        baseBytes = parseSizeToBytes(sz);
        baseDurSecs = parseDurSecs(ep.duration);
        break;
      }
    }

    // B. Check options.filesizes for activeRes
    if (!baseBytes && state.options?.filesizes?.[activeRes]) {
      baseBytes = parseSizeToBytes(state.options.filesizes[activeRes]);
      const ep0 = state.episodes[0] || Object.values(state.allKnownEpisodes)[0];
      baseDurSecs = parseDurSecs(ep0?.duration);
    }

    // C. Check if any other resolution has size in options.filesizes
    if (!baseBytes && state.options?.filesizes) {
      for (const [r, sz] of Object.entries(state.options.filesizes)) {
        if (sz) {
          const raw = parseSizeToBytes(sz);
          if (raw) {
            baseBytes = estimateSizeForResolution(raw, r, activeRes);
            const ep0 = state.episodes[0] || Object.values(state.allKnownEpisodes)[0];
            baseDurSecs = parseDurSecs(ep0?.duration);
            break;
          }
        }
      }
    }

    if (!baseBytes) return "";

    let totalBytes = 0;
    for (const id of state.episodesSelected) {
      const ep = state.allKnownEpisodes[id] || state.episodes.find(e => epId(e.episode) === id);
      const probed = state.episodeSizes?.[id]?.[activeRes];
      if (probed) {
        totalBytes += parseSizeToBytes(probed) || baseBytes;
      } else if (ep && baseDurSecs > 0) {
        const d = parseDurSecs(ep.duration);
        totalBytes += d > 0 ? Math.round(baseBytes * (d / baseDurSecs)) : baseBytes;
      } else {
        totalBytes += baseBytes;
      }
    }
    return formatBytes(totalBytes);
  }

  function probeFirstEpisode() {
    const ep = state.episodes[0];
    if (!ep) return;
    probeOptions(ep.episode);
  }

  async function probeOptions(episode) {
    if (!state.selectedSlug) return;
    const slug = state.selectedSlug;
    setBusy($("probeOptions"), true);
    try {
      const titleParam = state.selectedTitle ? `&title=${encodeURIComponent(state.selectedTitle)}` : "";
      const opts = await api.get(`/api/episode-options?slug=${encodeURIComponent(slug)}&episode=${encodeURIComponent(episode)}${titleParam}`);
      if (state.selectedSlug !== slug) return;
      state.options = opts;
      if (opts.filesizes) {
        const epKey = epId(opts.episode || episode);
        state.episodeSizes[epKey] = opts.filesizes;
      }
      fillOptionSelects();
      renderEpGrid();
      updateEpSelectionUI();
      updateCommandPreview();
    } catch (e) { /* silent */ }
    finally { setBusy($("probeOptions"), false); }
  }

  function optionPairs() {
    return Array.isArray(state.options.combinations) ? state.options.combinations : [];
  }

  function resolutionsForAudio(audio) {
    const pairs = optionPairs();
    if (!audio || !pairs.length) return state.options.resolutions;
    return state.options.resolutions.filter(resolution =>
      pairs.some(pair => pair.resolution === resolution && pair.audio === audio));
  }

  function audiosForResolution(resolution) {
    const pairs = optionPairs();
    if (!resolution || !pairs.length) return state.options.audios;
    return state.options.audios.filter(audio =>
      pairs.some(pair => pair.resolution === resolution && pair.audio === audio));
  }

  function renderResolutionOptions(audio, selected) {
    const select = $("optResolution");
    const values = resolutionsForAudio(audio);
    select.innerHTML = '<option value="">Auto — highest available</option>' +
      values.map(v => {
        const sz = state.options?.filesizes?.[v];
        const label = sz ? `${escapeHtml(v)}p (${escapeHtml(sz)})` : `${escapeHtml(v)}p`;
        return `<option value="${escapeHtml(v)}">${label}</option>`;
      }).join("");
    select.value = values.includes(selected) ? selected : "";
    return select.value;
  }

  function renderAudioOptions(resolution, selected) {
    const select = $("optAudio");
    const values = audiosForResolution(resolution);
    const tracks = Array.from(new Set(["", "jpn", "eng", ...values, ...state.availableAudios]));
    select.innerHTML = tracks.map(v => {
      if (!v) return '<option value="">Default (All)</option>';
      if (v === "jpn") return '<option value="jpn">Japanese (Sub)</option>';
      if (v === "eng") return '<option value="eng">English (Dub)</option>';
      return `<option value="${escapeHtml(v)}">${escapeHtml(v).toUpperCase()}</option>`;
    }).join("");
    select.value = tracks.includes(selected) ? selected : "";
    return select.value;
  }

  function updateOptionHints() {
    const audio = $("optAudio").value;
    const resolution = $("optResolution").value;
    const resolutions = resolutionsForAudio(audio);
    const audios = audiosForResolution(resolution);
    $("resolutionHint").textContent = resolutions.length
      ? `Available${audio ? ` with ${audio.toUpperCase()}` : ""}: ${resolutions.map(v => v + "p").join(", ")}`
      : "No resolution data — the backend will pick the highest available.";
    $("audioHint").textContent = audios.length
      ? `Available${resolution ? ` at ${resolution}p` : ""}: ${audios.map(v => v.toUpperCase()).join(", ")}`
      : "No audio track data — the source's own audio will be used.";
  }

  function fillOptionSelects() {
    const r = $("optResolution"), a = $("optAudio");
    const previousResolution = r.value;
    let preferredAudio = a.value;
    if (state.audioFilter === "dub" || state.audioFilter === "eng") preferredAudio = "eng";
    else if (state.audioFilter === "sub" || state.audioFilter === "jpn") preferredAudio = "jpn";

    // Rebuild both lists from the probe result. Preserve the chosen audio
    // first; if its resolution is gone, fall back to Auto within that audio.
    renderResolutionOptions("", "");
    renderAudioOptions("", preferredAudio);
    if (preferredAudio && state.options.audios.includes(preferredAudio)) {
      const keptResolution = renderResolutionOptions(preferredAudio, previousResolution);
      renderAudioOptions(keptResolution, preferredAudio);
    } else if (previousResolution && state.options.resolutions.includes(previousResolution)) {
      const keptAudio = renderAudioOptions(previousResolution, preferredAudio);
      renderResolutionOptions(keptAudio, previousResolution);
    }
    updateOptionHints();
  }

  /* ─────────────────────────────────────────────────────────────────
   *  episode selection helpers
   * ───────────────────────────────────────────────────────────────── */

  function toggleEpisode(ep) {
    const id = epId(ep);
    state.episodesSelected = state.episodesSelected.includes(id)
      ? state.episodesSelected.filter(x => x !== id)
      : [...state.episodesSelected, id];
    renderEpGrid();
    updateCommandPreview();
  }

  function selectAll() {
    const displayList = getDisplayEpisodes();
    state.episodesSelected = displayList.map(e => epId(e.episode));
    renderEpGrid();
    updateCommandPreview();
  }
  function selectNone() {
    state.episodesSelected = [];
    renderEpGrid();
    updateCommandPreview();
  }
  function invertSelection() {
    const displayList = getDisplayEpisodes();
    const all = displayList.map(e => epId(e.episode));
    state.episodesSelected = all.filter(ep => !state.episodesSelected.includes(ep));
    renderEpGrid();
    updateCommandPreview();
  }
  function selectRange() {
    const displayList = getDisplayEpisodes();
    if (!displayList.length) return;
    const first = prompt("First episode number:", epId(displayList[0].episode));
    if (!first) return;
    const last = prompt("Last episode number:", epId(displayList[displayList.length - 1].episode));
    if (!last) return;
    const a = parseInt(first, 10), b = parseInt(last, 10);
    if (isNaN(a) || isNaN(b)) return;
    const lo = Math.min(a, b), hi = Math.max(a, b);
    const availableIds = new Set(displayList.map(e => epId(e.episode)));
    const range = [];
    for (let n = lo; n <= hi; n++) {
      const s = String(n);
      if (availableIds.has(s)) range.push(s);
    }
    state.episodesSelected = [...new Set([...state.episodesSelected, ...range])];
    renderEpGrid();
    updateCommandPreview();
  }
  function selectLatest() {
    const displayList = getDisplayEpisodes();
    if (!displayList.length) return;
    const latest = displayList.slice(-5).map(e => epId(e.episode));
    state.episodesSelected = [...new Set([...state.episodesSelected, ...latest])];
    renderEpGrid();
    updateCommandPreview();
  }

  function updateEpSelectionUI() {
    const count = state.episodesSelected.length;
    $("epTitle").textContent = state.selectedTitle || state.selectedSlug || "Episodes";
    const displayList = getDisplayEpisodes();
    const shownCount = displayList.length;
    let base = `${shownCount} episodes`;
    if (state.audioFilter === "dub" || state.audioFilter === "eng") {
      base = `Showing ${shownCount} Dub episode${shownCount === 1 ? "" : "s"}${state.epTotalAll ? ` (${state.epTotalAll} total)` : ""}`;
    } else if (state.audioFilter === "sub" || state.audioFilter === "jpn") {
      base = `Showing ${shownCount} Sub episode${shownCount === 1 ? "" : "s"}${state.epTotalAll ? ` (${state.epTotalAll} total)` : ""}`;
    }
    const totalSize = calculateTotalSelectedSize();
    const sizeStr = totalSize ? ` (~${totalSize})` : "";
    $("epSubtitle").textContent = `${base} • ${count} selected${sizeStr}${count ? " — " + compressEpisodes(state.episodesSelected) : ""}`;
    $("startBtn").disabled = count === 0;

    const pill = $("epCapacityPill");
    if (pill) {
      if (count > 0 && totalSize) {
        pill.textContent = `📦 ~${totalSize} (${count} ep${count === 1 ? "" : "s"})`;
        pill.classList.remove("hidden");
      } else {
        pill.classList.add("hidden");
      }
    }

    updateActionSummary();
  }

  function compressEpisodes(arr) {
    if (!arr.length) return "";
    if (arr.some(n => String(n).includes(".") || isNaN(Number(n)))) {
      return arr.join(", ");
    }
    const nums = arr.map(n => parseInt(n, 10)).filter(n => !isNaN(n)).sort((a, b) => a - b);
    const parts = [];
    let start = nums[0], prev = nums[0];
    for (let i = 1; i <= nums.length; i++) {
      const cur = nums[i];
      if (i < nums.length && cur === prev + 1) { prev = cur; continue; }
      parts.push(start === prev ? String(start) : `${start}-${prev}`);
      start = prev = cur;
    }
    return parts.join(", ");
  }

  function updateCommandPreview() {
    if (!state.selectedSlug) {
      $("commandPreview").textContent = "Select an anime to preview the job";
      return;
    }
    const eps = state.episodesSelected.length ? compressEpisodes(state.episodesSelected) : "all episodes";
    const parts = [`Native engine: ${state.selectedSlug}`, `episodes ${eps}`];
    if ($("optResolution").value) parts.push(`${$("optResolution").value}p`);
    if ($("optAudio").value) parts.push($("optAudio").value.toUpperCase());
    if ($("optReplace").checked) parts.push("replace existing");
    if ($("optListOnly").checked) parts.push("playlist link only");
    if ($("optDebug").checked) parts.push("debug");
    parts.push(`${clampFragments(state.config.concurrent_fragments)} fragments`);
    $("commandPreview").textContent = parts.join(" · ");
  }

  function updateActionSummary() {
    const ep = state.job?.episodes || {};
    const isRunning = Boolean(state.job && (state.job.running || state.job.paused));
    const cur = ep.current ? "E" + ep.current : "—";
    const requestedAll = (ep.requested || []).includes("*");
    const total = requestedAll
      ? state.episodes.length
      : ep.requested?.length
        ? ep.requested.length
        : (ep.completed?.length || 0) + (ep.failed?.length || 0);

    $("actionAnime").textContent = state.selectedTitle || state.selectedSlug || "—";
    if (isRunning && total) {
      $("actionEpisodes").textContent = `${cur} / ${total}`;
    } else if (state.episodesSelected.length) {
      const totalSize = calculateTotalSelectedSize();
      const sizeStr = totalSize ? ` (~${totalSize})` : "";
      $("actionEpisodes").textContent = `${state.episodesSelected.length} selected${sizeStr}`;
    } else {
      $("actionEpisodes").textContent = "—";
    }

    const rVal = $("optResolution").value;
    const szVal = state.options?.filesizes?.[rVal];
    const res = rVal ? (szVal ? `${rVal}p (${szVal})` : `${rVal}p`) : "Auto";
    const aud = ($("optAudio").value || "default").toUpperCase();
    const replace = $("optReplace").checked ? " • replace" : "";
    $("actionQuality").textContent = `${res} • ${aud}${replace}`;
  }

  /* ─────────────────────────────────────────────────────────────────
   *  render: settings
   * ───────────────────────────────────────────────────────────────── */

  function renderSettings() {
    const c = state.config;
    $("cfgCf").value = "";
    $("cfgCf").placeholder = c.cfConfigured
      ? `Saved: ${c.cfLength} characters — leave blank to keep`
      : "Paste the cf_clearance cookie value";
    $("cfgUa").value = c.ua || "";
    $("cfgFrag").value = clampFragments(c.concurrent_fragments);
    $("cfgDownloadDir").value = c.downloadDir || c.defaultDownloadDir || "";
    updateBadges();
    updateDownloadDirBadge();
    renderDeps();
    renderSetup();
    const autoSolveCb = $("cfgAutoSolveOnLaunch");
    if (autoSolveCb) {
      autoSolveCb.checked = (localStorage.getItem("paheflow_auto_solve_cf_on_launch") ?? "false") !== "false";
      autoSolveCb.onchange = () => {
        localStorage.setItem("paheflow_auto_solve_cf_on_launch", autoSolveCb.checked ? "true" : "false");
      };
    }
    updateSaveHint("");
  }

  function renderSetup() {
    const panel = $("setupPanel");
    if (!panel) return;
    const configured = Boolean(state.config.cfConfigured);
    panel.classList.toggle("hidden", configured);
    const badge = $("setupBadge");
    badge.textContent = configured ? "configured" : "cookie needed";
    badge.className = "badge " + (configured ? "ok" : "warn");

    const missing = (state.deps || [])
      .filter(d => d.required && !d.found)
      .map(d => d.name);
    const hint = $("setupHint");
    if (window.AndroidBridge) {
      hint.textContent = "All dependencies are built directly into the Android app. No extra installation required!";
      hint.className = "option-hint";
    } else if (missing.length) {
      hint.textContent = `Missing required tools: ${missing.join(", ")}. Run ./requirements.sh, then restart the GUI.`;
      hint.className = "option-hint save-hint err";
    } else {
      hint.textContent = "The cookie is stored only in config.json on this computer and is never sent back to the browser.";
      hint.className = "option-hint";
    }
  }

  function useBrowserUa() {
    const ua = navigator.userAgent || "";
    if (!ua) { toast("This browser did not report a User-Agent", "warn"); return; }
    $("cfgUa").value = ua;
    const panel = $("setupPanel");
    if (panel && !panel.classList.contains("hidden")) {
      const hint = $("setupHint");
      hint.className = "option-hint";
      hint.textContent = "Browser User-Agent filled in. Paste the cookie below, then click Save & test connection.";
    }
    toast("This browser's User-Agent filled in — save to apply", "ok");
  }

  function focusCookieField() {
    showTab("settings");
    const input = $("cfgCf");
    input.focus();
    input.scrollIntoView({ behavior: "smooth", block: "center" });
  }

  function updateDownloadDirBadge() {
    const badge = $("downloadDirBadge");
    if (!badge) return;
    const current = state.config.downloadDir || "";
    const fallback = state.config.defaultDownloadDir || "";
    const isDefault = !current || !fallback || current === fallback;
    badge.textContent = isDefault ? "project default" : "custom";
    badge.className = "badge" + (isDefault ? "" : " accent");
  }

  function updateBadges() {
    const configured = Boolean(state.config.cfConfigured);
    const ua = state.config.ua;
    const cfBadge = $("cfBadge"), uaBadge = $("uaBadge");
    cfBadge.textContent = configured ? `${state.config.cfLength} chars` : "empty";
    cfBadge.className = "badge " + (configured ? "ok" : "err");
    uaBadge.textContent = ua ? "set" : "empty";
    uaBadge.className = "badge " + (ua ? "ok" : "warn");
  }

  const UA_PRESETS = [
    ["Firefox 158 (Linux)", "Mozilla/5.0 (X11; Linux x86_64; rv:158.0) Gecko/20100101 Firefox/158.0"],
    ["Firefox 135 (Windows)", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:135.0) Gecko/20100101 Firefox/135.0"],
    ["Chrome 132 (Linux)", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36"],
    ["Chrome 132 (Windows)", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36"],
    ["Safari 17 (macOS)", "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_4) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15"],
  ];

  function fillUAPresets() {
    const sel = $("uaPreset");
    if (!sel) return;
    const blank = '<option value="">' + "select a preset" + '</option>';
    sel.innerHTML = blank + UA_PRESETS
      .map(p => `<option value="${escapeHtml(p[1])}">${escapeHtml(p[0])}</option>`)
      .join("");
  }

  function renderDeps() {
    const box = $("depList");
    box.innerHTML = state.deps.map(d => `
      <div class="dep ${d.found ? "found" : ""}">
        <span class="dep-mark"></span>
        <span class="dep-name">${escapeHtml(d.name)}</span>
        <span class="dep-purpose">${escapeHtml(d.purpose)}</span>
        ${d.required ? '<span class="dep-tag required">required</span>' : ""}
        ${d.found && d.path ? `<span class="dep-tag">${escapeHtml(d.path)}</span>` : ""}
      </div>
    `).join("");
  }

  async function testConnection() {
    const btn = $("testConnection");
    setBusy(btn, true);
    try {
      const res = await api.post("/api/test-connection", {});
      toast(res.ok ? "Connection OK" : "Connection failed: " + res.message, res.ok ? "ok" : "error");
      if (res.ok) {
        $("cookieBanner").classList.add("hidden");
      } else {
        noteApiError(res.message);
      }
    } catch (e) {
      toast("Test failed: " + e.message, "error");
      noteApiError(e.message);
    }
    finally { setBusy(btn, false); }
  }

  async function saveConfig() {
    const cfValue = $("cfgCf").value.trim();
    const previousDir = state.config.downloadDir;
    const payload = {
      ua: $("cfgUa").value.trim(),
      concurrent_fragments: clampFragments($("cfgFrag").value),
      download_dir: $("cfgDownloadDir").value.trim(),
    };
    // Omitting the cookie means "keep the stored value"; the server never
    // sends it back, so an empty field must not wipe it out.
    if (cfValue) payload.cf = cfValue;
    try {
      const saved = await api.post("/api/config", payload);
      state.config = normalizeConfig(saved);
      renderSettings();
      updateCommandPreview();
      updateSaveHint("ok", "Saved");
      toast("Configuration saved", "ok");
      return true;
    } catch (e) {
      updateSaveHint("err", "Failed: " + e.message);
      toast("Save failed: " + e.message, "error");
      return false;
    }
  }

  async function autoDetectCookie() {
    const btn1 = $("autoDetectCf");
    const btn2 = $("setupAutoDetect");
    if (btn1) setBusy(btn1, true);
    if (btn2) setBusy(btn2, true);
    try {
      const res = await api.post("/api/auto-detect-cookie", {});
      if (res && res.ok) {
        state.config = normalizeConfig(res.config);
        renderSettings();
        toast("✨ Cloudflare cookie auto-detected from local browser!", "ok");
        await testConnection();
      } else {
        toast("No cookie found. Open animepahe.pw in your browser first!", "warn");
      }
    } catch (err) {
      toast(err.message || "Could not auto-detect cookie.", "err");
    } finally {
      if (btn1) setBusy(btn1, false);
      if (btn2) setBusy(btn2, false);
    }
  }

  /* ─────────────────────────────────────────────────────────────────
   *  cloudflare solver modal
   * ───────────────────────────────────────────────────────────────── */

  let cfSolverPollTimer = null;
  let cfSolverWindow = null;

  function openCfSolverModal() {
    if (window.AndroidBridge && typeof window.AndroidBridge.openCloudflareSolver === "function") {
      window.AndroidBridge.openCloudflareSolver();
      return;
    }
    const modal = $("cfSolverModal");
    if (!modal) return;
    modal.classList.remove("hidden");
    $("cfSolverStatusText").textContent = "Click 'Solve Cloudflare' to open the verification window.";
    $("cfSolverSpinner").classList.add("hidden");
    $("cfStartSolverBtn").disabled = false;
  }

  function closeCfSolverModal() {
    const modal = $("cfSolverModal");
    if (modal) modal.classList.add("hidden");
    if (cfSolverPollTimer) {
      clearInterval(cfSolverPollTimer);
      cfSolverPollTimer = null;
    }
    if (cfSolverWindow && !cfSolverWindow.closed) {
      cfSolverWindow.close();
      cfSolverWindow = null;
    }
  }

  /* ─────────────────────────────────────────────────────────────────
   *  shutdown modal & execution
   * ───────────────────────────────────────────────────────────────── */

  function openShutdownModal() {
    const modal = $("shutdownModal");
    if (!modal) return;
    modal.classList.remove("hidden");
  }

  function closeShutdownModal() {
    const modal = $("shutdownModal");
    if (!modal) return;
    modal.classList.add("hidden");
  }

  async function performShutdown() {
    const btn = $("confirmShutdownBtn");
    if (btn) {
      btn.disabled = true;
      btn.textContent = "Shutting down…";
    }

    if (window.AndroidBridge && typeof window.AndroidBridge.shutdownApp === "function") {
      window.AndroidBridge.shutdownApp();
      return;
    }

    try {
      await api.post("/api/shutdown");
    } catch (_) {}

    document.body.innerHTML = `
      <div style="display:flex;flex-direction:column;align-items:center;justify-content:center;height:100vh;background:#0b0813;color:#f0eef5;font-family:system-ui,-apple-system,sans-serif;text-align:center;padding:24px;">
        <div style="font-size:3rem;margin-bottom:12px;color:#ff4c6a;">⏻</div>
        <h2 style="font-size:1.4rem;font-weight:700;margin-bottom:8px;">PaheFlow has shut down</h2>
        <p style="color:#9f9baa;font-size:0.95rem;max-width:380px;">The background service has stopped. You can safely close this window.</p>
      </div>
    `;
  }

  async function startCfSolver() {
    // Clear any existing polling timer
    if (cfSolverPollTimer) {
      clearInterval(cfSolverPollTimer);
      cfSolverPollTimer = null;
    }

    if (window.AndroidBridge) {
      // On Android: close modal, open native solver
      closeCfSolverModal();
      if (typeof window.AndroidBridge.openInFirefox === "function") {
        window.AndroidBridge.openInFirefox();
      } else if (typeof window.AndroidBridge.openCloudflareSolver === "function") {
        window.AndroidBridge.openCloudflareSolver();
      }
      return;
    }

    const btn = $("cfStartSolverBtn");
    const statusText = $("cfSolverStatusText");
    const spinner = $("cfSolverSpinner");

    btn.disabled = true;
    spinner.classList.remove("hidden");
    statusText.textContent = "AnimePahe opened in Firefox. Complete the 'Verify you are human' check...";

    // Launch AnimePahe directly in Firefox via backend
    try {
      await api.post("/api/open-cf-page", {});
    } catch (e) {
      // Fallback: open standard new tab
      try {
        cfSolverWindow = window.open("https://animepahe.pw/", "_blank");
      } catch (err) {
        // Ignored
      }
    }

    if (cfSolverPollTimer) clearInterval(cfSolverPollTimer);

    let attempts = 0;
    cfSolverPollTimer = setInterval(async () => {
      attempts++;
      try {
        const res = await api.post("/api/auto-detect-cookie", {});
        if (res && res.ok && res.config) {
          state.config = normalizeConfig(res.config);
          renderSettings();

          const testRes = await api.post("/api/test-connection", {});
          if (testRes && testRes.ok) {
            clearInterval(cfSolverPollTimer);
            cfSolverPollTimer = null;

            spinner.classList.add("hidden");
            statusText.textContent = "✅ Cloudflare verified! Connected!";
            toast("Cloudflare verified and auto-configured from Firefox!", "ok");
            
            // Show green "solved" state on buttons
            highlightSolveCfButton("solved");

            if (cfSolverWindow && !cfSolverWindow.closed) {
              cfSolverWindow.close();
              cfSolverWindow = null;
            }

            setTimeout(() => {
              closeCfSolverModal();
              $("cookieBanner").classList.add("hidden");
              renderCatalog();
            }, 800);
            return;
          }
        }
      } catch (e) {
        // Continue polling
      }

      if (attempts > 90) { // 90 * 800ms = ~72s timeout
        clearInterval(cfSolverPollTimer);
        cfSolverPollTimer = null;
        spinner.classList.add("hidden");
        btn.disabled = false;
        statusText.textContent = "Timeout waiting for verification. Click to try again.";
      }
    }, 800);
  }

  async function setupSaveAndTest() {
    const btn = $("setupSaveTest");
    setBusy(btn, true);
    try {
      const cookie = $("cfgCf").value.trim();
      const uaChanged = $("cfgUa").value.trim() !== (state.config.ua || "");
      if (cookie || uaChanged) {
        const saved = await saveConfig();
        if (!saved) return;
      }
      await testConnection();
    } finally {
      setBusy(btn, false);
    }
  }

  async function browseDownloadDir() {
    const btn = $("browseDownloadDir");
    setBusy(btn, true);
    try {
      if (window.AndroidBridge && typeof window.AndroidBridge.openFolderPicker === "function") {
        window.AndroidBridge.openFolderPicker();
        return;
      }
      const choice = await api.post("/api/pick-folder", {
        current: $("cfgDownloadDir").value.trim(),
      });
      if (choice.cancelled) return;
      const saved = await api.post("/api/config", { download_dir: choice.path });
      state.config = normalizeConfig(saved);
      $("cfgDownloadDir").value = state.config.downloadDir;
      updateDownloadDirBadge();
      updateSaveHint("");
      toast("Download folder saved", "ok");
    } catch (e) {
      toast("Could not choose folder: " + e.message, "error");
    } finally {
      if (!window.AndroidBridge || typeof window.AndroidBridge.openFolderPicker !== "function") {
        setBusy(btn, false);
      }
    }
  }

  window.onFolderPicked = async function(chosenPath) {
    const btn = $("browseDownloadDir");
    if (btn) setBusy(btn, false);
    if (!chosenPath) return;
    try {
      const saved = await api.post("/api/config", { download_dir: chosenPath });
      state.config = normalizeConfig(saved);
      $("cfgDownloadDir").value = state.config.downloadDir;
      updateDownloadDirBadge();
      updateSaveHint("");
      toast("Download folder saved: " + chosenPath, "ok");
    } catch (e) {
      toast("Could not save download folder: " + e.message, "error");
    }
  };

  async function resetDownloadDir() {
    if (!confirm("Reset download folder to default (Downloads/PaheFlow)?")) return;
    const btn = $("resetDownloadDir");
    setBusy(btn, true);
    try {
      const saved = await api.post("/api/config", { download_dir: "" });
      state.config = normalizeConfig(saved);
      $("cfgDownloadDir").value = state.config.downloadDir;
      updateDownloadDirBadge();
      updateSaveHint("");
      toast("Download folder reset to default", "ok");
    } catch (e) {
      toast("Could not reset the download folder: " + e.message, "error");
    } finally {
      setBusy(btn, false);
    }
  }

  async function clearCookie() {
    if (!confirm("Delete the stored cf_clearance cookie?")) return;
    try {
      const saved = await api.post("/api/config", { cf: "" });
      state.config = normalizeConfig(saved);
      renderSettings();
      toast("Cookie cleared", "ok");
    } catch (e) {
      toast("Could not clear cookie: " + e.message, "error");
    }
  }

  function updateSaveHint(kind, msg) {
    const el = $("saveHint");
    el.className = "save-hint " + (kind || "");
    el.textContent = msg || "";
  }

  function reloadConfig() {
    loadInitialState();
  }

  async function copyConfig() {
    const text = JSON.stringify({
      ua: state.config.ua,
      concurrent_fragments: state.config.concurrent_fragments,
      download_dir: state.config.downloadDir,
    }, null, 2);
    await navigator.clipboard.writeText(text);
    toast("Non-secret settings copied", "ok");
  }

  function copyLogs() {
    const lines = $$(".log-line", $("logs")).map(el => el.querySelector(".log-text")?.textContent ?? "").join("\n");
    navigator.clipboard.writeText(lines);
    toast("Logs copied", "ok");
  }

  async function clearLogs() {
    try {
      await api.post("/api/clear-logs", {});
      logQueue = [];
      if (logFrame !== null) { cancelAnimationFrame(logFrame); logFrame = null; }
      state.lastLogSeq = Math.max(state.lastLogSeq, state.logSeq);
      $("logs").innerHTML = "";
      toast("Activity log cleared", "ok");
    } catch (e) {
      toast("Could not clear log: " + e.message, "error");
    }
  }

  /* ─────────────────────────────────────────────────────────────────
   *  job / activity panel
   * ───────────────────────────────────────────────────────────────── */

  function applyJob(job) {
    state.job = job;
    updateActionSummary();
    updateProgressUI();
    updateEpStatus();
    updatePlaylistBlock();
  }

  function updateProgressUI() {
    updateActionBarVisibility();
    if (!state.job || !state.job.running) {
      $("progressLabel").textContent = "Idle";
      $("progressPercent").textContent = "—";
      $("progressFill").style.width = "0%";
      $("progressFill").classList.remove("is-paused");
      $("progressEpisode").textContent = "";
      $("progressSpeed").textContent = "";
      $("progressEta").textContent = "";
      updatePill("idle");
      return;
    }
    const p = state.job.progress || {};
    if (state.job.paused) {
      $("progressLabel").textContent = "Paused";
      $("progressFill").classList.add("is-paused");
      $("progressSpeed").textContent = "⏸ Paused";
      updatePill("paused");
    } else {
      $("progressFill").classList.remove("is-paused");
      if (p.percent !== null && p.percent !== undefined) {
        $("progressFill").style.width = Math.max(0, Math.min(100, p.percent)) + "%";
        $("progressPercent").textContent = p.percent.toFixed(1) + "%";
        $("progressLabel").textContent = "Downloading…";
      } else {
        $("progressFill").style.width = "0%";
        $("progressPercent").textContent = "—";
        $("progressLabel").textContent = state.job.progress?.file ? "Starting…" : "Downloading…";
      }
      $("progressSpeed").textContent = state.job.progress?.speed ? "⚡ " + state.job.progress.speed : "";
      updatePill("running");
    }
    $("progressEpisode").textContent = state.job.progress?.file ? "💾 " + state.job.progress.file.split("/").pop() : "";
    $("progressEta").textContent = state.job.progress?.eta ? "⏱ " + state.job.progress.eta : "";
  }

  function updateEpStatus() {
    if (!state.job) {
      if (state.epStatusKey !== "") {
        state.epStatusKey = "";
        $("epStatus").innerHTML = "";
      }
      return;
    }
    const ep = state.job.episodes || {};
    const key = [(ep.completed || []).join(","), (ep.failed || []).join(","), ep.current || ""].join("|");
    if (key === state.epStatusKey) return;
    state.epStatusKey = key;
    const chips = [];
    (ep.completed || []).forEach(e => chips.push(`<span class="ep-chip">E${escapeHtml(e)} ✓</span>`));
    (ep.failed || []).forEach(e => chips.push(`<span class="ep-chip failed">E${escapeHtml(e)} ✗</span>`));
    if (ep.current && !(ep.completed || []).includes(ep.current)) chips.push(`<span class="ep-chip">E${escapeHtml(ep.current)} ↻</span>`);
    $("epStatus").innerHTML = chips.join("");
  }

  function updatePlaylistBlock() {
    const playlists = state.job?.playlists || [];
    if (!state.job || !playlists.length) {
      if (state.playlistKey !== "") {
        state.playlistKey = "";
        $("playlistBlock").classList.add("hidden");
      }
      return;
    }
    const key = playlists.join("\n");
    if (key === state.playlistKey) return;
    state.playlistKey = key;
    $("playlistBlock").classList.remove("hidden");
    $("playlistList").innerHTML = playlists.map((u) => `
      <div class="playlist-item">
        <code>${escapeHtml(u)}</code>
        <button class="btn btn-ghost btn-sm" data-copy="${escapeHtml(u)}" title="Copy">Copy</button>
      </div>
    `).join("");
    $$(".playlist-item button", $("playlistList")).forEach(btn => btn.onclick = () => copyToClip(btn.dataset.copy, "Playlist copied"));
  }

  function copyToClip(text, msg) {
    navigator.clipboard.writeText(text);
    toast(msg, "ok");
  }

  function updatePill(st) {
    const el = $("statusPill"), txt = $("statusText");
    el.dataset.state = st;
    switch (st) {
      case "running": txt.textContent = "Running"; break;
      case "paused": txt.textContent = "Paused"; break;
      case "done": txt.textContent = "Done"; break;
      case "error": txt.textContent = "Error"; break;
      default: txt.textContent = "Idle";
    }
  }

  function handleState(msg) {
    applyJob(msg);
    if (typeof msg.logSeq === "number") state.logSeq = msg.logSeq;
    if (msg.running) {
      $("stopBtn").disabled = false;
      if (msg.paused) {
        $("startBtn").textContent = "Resume";
        $("startBtn").disabled = false;
        updatePill("paused");
      } else {
        $("startBtn").textContent = "Pause";
        $("startBtn").disabled = false;
        updatePill("running");
      }
      return;
    }
    $("startBtn").textContent = "Start download";
    if (msg.exitCode === null || msg.exitCode === undefined) return;
    updatePill(msg.stopped ? "idle" : (msg.exitCode === 0 ? "done" : "error"));
    $("startBtn").disabled = state.episodesSelected.length === 0;
    $("stopBtn").disabled = true;
  }

  function handleDone(msg) {
    handleState(msg);
    const key = `${msg.startedAt || ""}:${msg.exitCode}:${msg.finishedAt || ""}`;
    if (state.lastDoneJob === key) return;
    state.lastDoneJob = key;
    if (msg.stopped) {
      toast("Download stopped", "warn");
    } else {
      const ok = msg.exitCode === 0;
      toast(ok ? "Download finished" : "Download failed (exit " + msg.exitCode + ")", ok ? "ok" : "error");
    }
  }

  function logMatchesFilter(level) {
    return state.logFilter === "all" || level === state.logFilter;
  }

  function applyLogFilter() {
    $$(".log-line", $("logs")).forEach(line => {
      line.style.display = logMatchesFilter(line.dataset.level) ? "" : "none";
    });
  }

  function handleLog(entry) {
    if (!entry || typeof entry.text !== "string") return;
    const seq = Number(entry.seq);
    if (Number.isFinite(seq)) {
      // EventSource is ordered; compare against the newest sequence instead of
      // scanning the DOM for a duplicate on every high-frequency log line.
      if (seq <= state.lastLogSeq) return;
      state.lastLogSeq = seq;
      state.logSeq = Math.max(state.logSeq, seq);
    }
    logQueue.push(entry);
    if (logQueue.length > MAX_QUEUED_LOGS) {
      logQueue.splice(0, logQueue.length - MAX_QUEUED_LOGS);
    }
    if (logFrame === null) logFrame = requestAnimationFrame(flushLogQueue);
  }

  function flushLogQueue() {
    logFrame = null;
    if (!logQueue.length) return;
    const batch = logQueue.length > MAX_LOG_LINES
      ? logQueue.slice(-MAX_LOG_LINES)
      : logQueue;
    logQueue = [];
    const logs = $("logs");
    const fragment = document.createDocumentFragment();
    for (const entry of batch) {
      const level = entry.level || "plain";
      const div = document.createElement("div");
      div.className = `log-line ${level}`;
      div.dataset.seq = entry.seq ?? "";
      div.dataset.level = level;
      const time = document.createElement("span");
      time.className = "log-time";
      time.textContent = fmtTime((entry.ts ?? entry.time ?? Date.now() / 1000) * 1000);
      const text = document.createElement("span");
      text.className = "log-text";
      text.textContent = entry.text;
      div.append(time, text);
      div.style.display = logMatchesFilter(level) ? "" : "none";
      fragment.appendChild(div);
    }
    logs.appendChild(fragment);
    // Older lines are intentionally discarded. Keeping every progress line
    // makes a long batch progressively slower and eventually freezes the tab.
    while (logs.childElementCount > MAX_LOG_LINES) {
      logs.firstElementChild.remove();
    }
    if (state.autoscroll) logs.scrollTop = logs.scrollHeight;
  }

  function startJob() {
    if (!state.selectedSlug) { toast("Select an anime first", "warn"); return; }
    if (window.AndroidBridge && typeof window.AndroidBridge.hasStoragePermission === "function") {
      if (!window.AndroidBridge.hasStoragePermission()) {
        const banner = $("storagePermissionBanner");
        if (banner) banner.classList.remove("hidden");
      }
    }
    const episodes = state.episodesSelected.length ? compressEpisodes(state.episodesSelected) : "*";
    const payload = {
      slug: state.selectedSlug,
      title: state.selectedTitle,
      episodes,
      resolution: $("optResolution").value.trim(),
      audio: $("optAudio").value.trim(),
      concurrent_fragments: clampFragments(state.config.concurrent_fragments),
      replace: $("optReplace").checked,
      listOnly: $("optListOnly").checked,
      debug: $("optDebug").checked,
    };
    api.post("/api/run", payload).then(res => {
      applyJob(res.job);
      state.lastDoneJob = "";
      // Remove the previous job's lines, but keep any new lines that the SSE
      // stream already delivered while this request was in flight.
      const clearThrough = Number(res.job.logSeq || 0);
      logQueue = logQueue.filter(entry => Number(entry.seq || 0) > clearThrough);
      state.lastLogSeq = Math.max(state.lastLogSeq, clearThrough);
      $$(".log-line", $("logs")).forEach(line => {
        if (Number(line.dataset.seq || 0) <= clearThrough) line.remove();
      });
      $("startBtn").textContent = "Pause";
      $("startBtn").disabled = false;
      $("stopBtn").disabled = false;
      updatePill("running");
    }).catch(e => toast("Start failed: " + e.message, "error"));
  }

  function pauseJob() {
    api.post("/api/pause", {}).then(() => {
      if (state.job) state.job.paused = true;
      $("startBtn").textContent = "Resume";
      updatePill("paused");
      updateProgressUI();
      toast("Download paused", "warn");
    }).catch(e => toast("Pause failed: " + e.message, "error"));
  }

  function resumeJob() {
    api.post("/api/resume", {}).then(() => {
      if (state.job) state.job.paused = false;
      $("startBtn").textContent = "Pause";
      updatePill("running");
      updateProgressUI();
      toast("Download resumed", "ok");
    }).catch(e => toast("Resume failed: " + e.message, "error"));
  }

  function handleMainAction() {
    if (!state.job?.running) {
      startJob();
    } else if (state.job?.paused) {
      resumeJob();
    } else {
      pauseJob();
    }
  }

  function stopJob() {
    api.post("/api/stop", {}).then(() => {
      if (state.job) {
        state.job.running = false;
        state.job.paused = false;
      }
      $("startBtn").textContent = "Start download";
      $("startBtn").disabled = state.episodesSelected.length === 0;
      $("stopBtn").disabled = true;
      updatePill("idle");
      updateProgressUI();
    }).catch(e => toast("Stop failed: " + e.message, "error"));
  }

  /* ─────────────────────────────────────────────────────────────────
   *  tab navigation
   * ───────────────────────────────────────────────────────────────── */

  function updateActionBarVisibility() {
    const isJobActive = Boolean(state.job && (state.job.running || state.job.paused));
    const show = isJobActive || (state.activeTab || "browse") === "episodes";
    const bar = document.querySelector(".actionbar");
    if (bar) {
      bar.classList.toggle("hidden", !show);
    }
  }

  function showTab(name) {
    if (name !== "browse" && state.catalogObserver) {
      state.catalogObserver.disconnect();
      state.catalogObserver = null;
    } else if (name === "browse" && !state.catalogObserver) {
      if (state.searchMode === "recent" && $("recentSentinel")) {
        setupRecentObserver(state.recentItems || []);
      } else if ($("catalogSentinel")) {
        setupCatalogObserver(filteredCatalog(), state.searchMode === "local");
      }
    }
    // Track tab history for back button navigation
    if (state.activeTab !== name) {
      state.tabHistory.push(state.activeTab);
      // Limit history size
      if (state.tabHistory.length > 20) state.tabHistory.shift();
      state.activeTab = name;
    }
    $$(".panel").forEach(p => p.classList.toggle("is-active", p.id === "panel-" + name));
    $$(".tab").forEach(t => t.classList.toggle("is-active", t.dataset.tab === name));
    updateActionBarVisibility();
  }

  /* ─────────────────────────────────────────────────────────────────
   *  boot / initial load
   * ───────────────────────────────────────────────────────────────── */

  async function loadInitialState() {
    try {
      const init = await api.get("/api/state");
      state.config = normalizeConfig(init.config);
      state.deps = init.dependencies;
      state.localCatalog = init.catalog?.items ?? [];
      state.catalogLimit = CATALOG_PAGE_SIZE;
      renderSettings();
      renderCatalog();

      // Test connection on startup
      await checkConnectionAndUpdateUI();

      // Default to false - only auto-solve if user explicitly enabled it
      const autoSolveOnLaunch = (localStorage.getItem("paheflow_auto_solve_cf_on_launch") ?? "false") !== "false";
      if (!state.config.cfConfigured) {
        // Don't auto-open - just highlight the button
        highlightSolveCfButton(true);
      } else if (autoSolveOnLaunch) {
        // Only auto-open if explicitly enabled AND connection failed
        setTimeout(async () => {
          // Check connection again before auto-opening
          const connected = await testConnection();
          if (!connected) {
            openCfSolverModal();
          }
        }, 350);
      }
      applyJob(init.job);
      if (init.job?.logSeq !== undefined) state.logSeq = init.job.logSeq;
      if (init.job?.running) {
        $("stopBtn").disabled = false;
        if (init.job?.paused) {
          $("startBtn").textContent = "Resume";
          $("startBtn").disabled = false;
          updatePill("paused");
        } else {
          $("startBtn").textContent = "Pause";
          $("startBtn").disabled = false;
          updatePill("running");
        }
      } else {
        $("startBtn").textContent = "Start download";
        if (init.job?.exitCode !== null && init.job?.exitCode !== undefined) {
          updatePill(init.job.stopped ? "idle" : (init.job.exitCode === 0 ? "done" : "error"));
        }
        $("startBtn").disabled = state.episodesSelected.length === 0;
        $("stopBtn").disabled = true;
      }
      updateActionBarVisibility();
      checkAppUpdates(true);
    } catch (e) {
      toast("Failed to load initial state: " + e.message, "error");
    }
  }

  function toggleActivityPanel(forceState = null) {
    const isMobile = window.innerWidth <= 900;
    if (forceState !== null) {
      state.activityVisible = forceState;
    } else {
      state.activityVisible = !state.activityVisible;
    }

    if (isMobile) {
      document.body.classList.toggle("show-mobile-activity", state.activityVisible);
      document.body.classList.remove("activity-hidden");
      const layout = $("layout");
      if (layout) layout.classList.remove("activity-hidden");
    } else {
      document.body.classList.remove("show-mobile-activity");
      document.body.classList.toggle("activity-hidden", !state.activityVisible);
      const layout = $("layout");
      if (layout) layout.classList.toggle("activity-hidden", !state.activityVisible);
    }

    const btn = $("toggleActivity");
    if (btn) {
      btn.classList.toggle("is-active", state.activityVisible);
      btn.title = state.activityVisible ? "Hide activity panel" : "Show activity panel";
      btn.setAttribute("aria-expanded", String(state.activityVisible));
    }
  }
  window.toggleActivityPanel = toggleActivityPanel;

  // Handle back button/gesture - returns action taken:
  // "handled" - back was handled (closed panel, went back in tab history, or web history)
  // "exit" - no more history, app should show exit confirmation
  function handleBackPress() {
    // 1. Close mobile activity panel if open
    const isMobile = window.innerWidth <= 900;
    if (isMobile && document.body.classList.contains("show-mobile-activity")) {
      toggleActivityPanel(false);
      return "handled";
    }

    // 2. Close any open modals first
    const modals = document.querySelectorAll(".modal-overlay:not(.hidden), .dialog-overlay:not(.hidden)");
    if (modals.length > 0) {
      modals.forEach(m => m.classList.add("hidden"));
      return "handled";
    }

    // 3. Go back in tab history if available
    if (state.tabHistory.length > 1) {
      const previousTab = state.tabHistory[state.tabHistory.length - 2];
      state.tabHistory.pop();
      state.activeTab = previousTab;
      showTab(previousTab);
      return "handled";
    }

    // 4. Check if WebView can go back (handled by Android side via webView.canGoBack())
    // We'll return "webview" to let Android handle web navigation
    return "webview";
  }

  window.handleBackPress = handleBackPress;

  /* ─────────────────────────────────────────────────────────────────
   *  bindings
   * ───────────────────────────────────────────────────────────────── */

  function bind() {
    // tabs
    $$(".tab").forEach(btn => btn.onclick = () => showTab(btn.dataset.tab));

    // delegated clicks for results list
    const resultsList = $("resultsList");
    if (resultsList) {
      resultsList.addEventListener("click", (e) => {
        const card = e.target.closest(".result");
        if (!card) return;
        if (card.classList.contains("recent")) {
          selectRecentEpisode(card.dataset.session, card.dataset.title, card.dataset.ep);
        } else if (card.dataset.slug) {
          selectAnime(card.dataset.slug);
        }
      });
    }

    // delegated clicks for episode grid
    const epGrid = $("epGrid");
    if (epGrid) {
      epGrid.addEventListener("click", (e) => {
        const el = e.target.closest(".ep");
        if (!el) return;
        const ep = el.dataset.ep;
        if (ep) toggleEpisode(ep);
      });
    }

    // search
    $("searchBtn").onclick = doSearch;
    $("searchInput").addEventListener("keydown", e => { if (e.key === "Enter") doSearch(); });
    $("searchInput").addEventListener("input", debounce(() => {
      if (state.searchMode === "local") renderCatalog();
    }, 150));
    $("clearSearch").onclick = () => { $("searchInput").value = ""; setCatalogMode("local"); };
    $$("#resultModes .seg").forEach(btn => btn.onclick = () => setCatalogMode(btn.dataset.mode));
    $("refreshCatalog").onclick = refreshCatalog;

    // episodes
    $("reloadEpisodes").onclick = () => loadEpisodes(state.selectedSlug, 1, state.audioFilter);
    $("epFilter").addEventListener("input", debounce(() => { state.epFilter = $("epFilter").value; renderEpGrid(); }));
    $$("#epAudioSegmented .seg").forEach(btn => {
      btn.onclick = () => {
        const audio = btn.dataset.audio || "";
        if (state.audioFilter === audio) return;
        state.audioFilter = audio;
        const displayList = getDisplayEpisodes();
        const validIds = new Set(displayList.map(e => epId(e.episode)));
        state.episodesSelected = state.episodesSelected.filter(id => validIds.has(id));
        syncAudioControlUI();
        renderEpGrid();
        loadEpisodes(state.selectedSlug, 1, audio);
      };
    });
    $$(".toolbar-buttons [data-ep]").forEach(btn => {
      btn.onclick = () => {
        switch (btn.dataset.ep) {
          case "all": selectAll(); break;
          case "none": selectNone(); break;
          case "invert": invertSelection(); break;
          case "range": selectRange(); break;
          case "latest": selectLatest(); break;
        }
      };
    });

    // options
    $("optResolution").onchange = () => {
      // Keep both selects on real source pairs: choosing a resolution narrows
      // the audio list, and an unavailable audio resets to Default.
      const audio = renderAudioOptions($("optResolution").value, $("optAudio").value);
      renderResolutionOptions(audio, $("optResolution").value);
      renderEpGrid();
      updateEpSelectionUI();
      updateOptionHints();
      updateActionSummary();
      updateCommandPreview();
    };
    $("optAudio").onchange = () => {
      const chosen = $("optAudio").value;
      let newFilter = "";
      if (chosen === "eng") newFilter = "dub";
      else if (chosen === "jpn") newFilter = "sub";

      if (state.audioFilter !== newFilter) {
        state.audioFilter = newFilter;
        const displayList = getDisplayEpisodes();
        const validIds = new Set(displayList.map(e => epId(e.episode)));
        state.episodesSelected = state.episodesSelected.filter(id => validIds.has(id));
        syncAudioControlUI();
        renderEpGrid();
        loadEpisodes(state.selectedSlug, 1, newFilter);
      }
      // Choosing audio narrows resolutions; an unavailable resolution resets
      // to Auto so the selected audio track is preserved.
      const resolution = renderResolutionOptions($("optAudio").value, $("optResolution").value);
      renderAudioOptions(resolution, $("optAudio").value);
      updateOptionHints();
      updateActionSummary();
      updateCommandPreview();
    };
    try {
      const savedReplace = localStorage.getItem("animepahe_replace");
      if (savedReplace !== null) {
        $("optReplace").checked = savedReplace === "true";
      }
    } catch (_) {}
    $("optReplace").onchange = () => {
      try {
        localStorage.setItem("animepahe_replace", String($("optReplace").checked));
      } catch (_) {}
      updateActionSummary();
      updateCommandPreview();
    };
    $("optListOnly").onchange = updateCommandPreview;
    $("optDebug").onchange = updateCommandPreview;
    $("probeOptions").onclick = () => probeOptions(state.options?.episode || state.episodes[0]?.episode || 1);

    // command
    $("copyCommand").onclick = () => copyToClip($("commandPreview").textContent, "Command copied");

    // settings
    $("saveConfig").onclick = saveConfig;
    $("reloadConfig").onclick = reloadConfig;
    $("testConnection").onclick = testConnection;
    $("copyConfig").onclick = copyConfig;
    $("clearLogs").onclick = clearLogs;
    $("cfgFragMinus").onclick = () => { $("cfgFrag").value = Math.max(1, clampFragments($("cfgFrag").value) - 1); };
    $("cfgFragPlus").onclick = () => { $("cfgFrag").value = Math.min(64, clampFragments($("cfgFrag").value) + 1); };
    $("cfgFrag").onchange = () => { $("cfgFrag").value = clampFragments($("cfgFrag").value); };
    $("toggleCf").onclick = () => { const i = $("cfgCf"); i.type = i.type === "password" ? "text" : "password"; $("toggleCf").textContent = i.type === "password" ? "Show" : "Hide"; };
    $("pasteCf").onclick = async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (!text.trim()) { toast("Clipboard is empty", "warn"); return; }
        $("cfgCf").value = text.trim();
        toast("Cookie pasted — click Save configuration", "ok");
      } catch (e) {
        toast("Clipboard access denied — paste with Ctrl+V instead", "warn");
        $("cfgCf").focus();
      }
    };
    $("clearCf").onclick = clearCookie;
    if ($("autoDetectCf")) $("autoDetectCf").onclick = autoDetectCookie;
    if ($("setupAutoDetect")) $("setupAutoDetect").onclick = autoDetectCookie;
    if ($("topbarSolveCf")) $("topbarSolveCf").onclick = openCfSolverModal;
    if ($("bannerSolveCookie")) $("bannerSolveCookie").onclick = openCfSolverModal;
    if ($("cfStartSolverBtn")) $("cfStartSolverBtn").onclick = startCfSolver;
    if ($("cfCancelSolverBtn")) $("cfCancelSolverBtn").onclick = closeCfSolverModal;
    if ($("shutdownBtnTop")) $("shutdownBtnTop").onclick = openShutdownModal;
    if ($("shutdownBtnSettings")) $("shutdownBtnSettings").onclick = openShutdownModal;
    if ($("confirmShutdownBtn")) $("confirmShutdownBtn").onclick = performShutdown;
    if ($("cancelShutdownBtn")) $("cancelShutdownBtn").onclick = closeShutdownModal;
    const shutdownModal = $("shutdownModal");
    if (shutdownModal) {
      shutdownModal.onclick = (e) => {
        if (e.target === shutdownModal) closeShutdownModal();
      };
    }
    $("browseDownloadDir").onclick = browseDownloadDir;
    $("resetDownloadDir").onclick = resetDownloadDir;
    $("uaPreset").onchange = (e) => { if (e.target.value) $("cfgUa").value = e.target.value; };
    $("useBrowserUa").onclick = useBrowserUa;
    $("setupUseBrowserUa").onclick = useBrowserUa;
    $("setupFocusCookie").onclick = focusCookieField;
    $("setupSaveTest").onclick = setupSaveAndTest;

    // activity
    const toggleBtn = $("toggleActivity");
    if (toggleBtn) {
      toggleBtn.onclick = () => toggleActivityPanel();
      toggleBtn.classList.toggle("is-active", state.activityVisible);
      toggleBtn.setAttribute("aria-expanded", String(state.activityVisible));
    }
    const statusPill = $("statusPill");
    if (statusPill) {
      statusPill.onclick = () => toggleActivityPanel();
      statusPill.style.cursor = "pointer";
    }
    const closeBtn = $("closeActivity");
    if (closeBtn) {
      closeBtn.onclick = () => toggleActivityPanel(false);
    }
    $$(".log-filters .chip").forEach(btn => btn.onclick = () => {
      state.logFilter = btn.dataset.level;
      $$(".log-filters .chip").forEach(b => b.classList.toggle("is-active", b === btn));
      applyLogFilter();
    });
    $("autoscroll").onchange = e => { state.autoscroll = e.target.checked; };
    $("clearLogsTop").onclick = clearLogs;
    $("copyLogs").onclick = copyLogs;
    $("copyPlaylists").onclick = () => copyToClip(state.job?.playlists?.join("\n") || "", "All playlist links copied");

    // actions
    $("startBtn").onclick = handleMainAction;
    $("stopBtn").onclick = stopJob;

    // copy slug
    $("copySlug").onclick = () => { if (state.selectedSlug) copyToClip(state.selectedSlug, "Slug copied"); };

    // cookie banner
    $("bannerSettings").onclick = () => showTab("settings");
    $("bannerClose").onclick = () => $("cookieBanner").classList.add("hidden");

    // storage permission banner
    if ($("grantStoragePermBtn")) {
      $("grantStoragePermBtn").onclick = () => {
        if (window.AndroidBridge && typeof window.AndroidBridge.requestStoragePermission === "function") {
          window.AndroidBridge.requestStoragePermission();
        }
      };
    }
    if ($("storagePermClose")) {
      $("storagePermClose").onclick = () => {
        sessionStorage.setItem("storage_banner_dismissed", "1");
        const banner = $("storagePermissionBanner");
        if (banner) banner.classList.add("hidden");
      };
    }
    checkAndroidStoragePermission();

    // auto update banner & modal
    if ($("btnUpdateNow")) {
      $("btnUpdateNow").onclick = () => openUpdateModal(latestUpdateInfo);
    }
    if ($("updateBannerClose")) {
      $("updateBannerClose").onclick = () => {
        if (latestUpdateInfo) {
          sessionStorage.setItem("paheflow_update_dismissed_" + latestUpdateInfo.latest_version, "1");
        }
        hideUpdateBanner();
      };
    }
    if ($("closeUpdateModalBtn")) {
      $("closeUpdateModalBtn").onclick = closeUpdateModal;
    }
    if ($("dismissUpdateModalBtn")) {
      $("dismissUpdateModalBtn").onclick = closeUpdateModal;
    }
    if ($("confirmUpdateBtn")) {
      $("confirmUpdateBtn").onclick = performAppUpdate;
    }
    if ($("btnCheckUpdates")) {
      $("btnCheckUpdates").onclick = () => checkAppUpdates(false);
    }

    // keyboard shortcuts
    document.addEventListener("keydown", (e) => {
      const tag = document.activeElement?.tagName || "";
      if (e.key === "/" && !["INPUT", "TEXTAREA", "SELECT"].includes(tag)) {
        e.preventDefault();
        $("searchInput").focus();
        $("searchInput").select();
      } else if (e.key === "Escape" && document.activeElement === $("searchInput")) {
        $("searchInput").value = "";
        setCatalogMode("local");
      }
    });
  }

  /* ─────────────────────────────────────────────────────────────────
   *  helpers
   * ───────────────────────────────────────────────────────────────── */

  function setBusy(btn, busy) {
    if (!btn) return;
    btn.disabled = busy;
    btn.dataset.busy = busy ? "1" : "";
  }

  /* ─────────────────────────────────────────────────────────────────
   *  theme picker — presentation only
   *
   *  Sets data-theme on <html> and remembers the choice. Reads and writes
   *  nothing else: no app state, no API calls, no event interference.
   * ───────────────────────────────────────────────────────────────── */

  function initThemePicker() {
    const select = $("themeSelect");
    if (!select) return;
    const KEY = "apgui-theme";
    let saved = null;
    try { saved = localStorage.getItem(KEY); } catch (e) { /* storage unavailable */ }
    if (saved && [...select.options].some(o => o.value === saved)) select.value = saved;
    const apply = () => { document.documentElement.dataset.theme = select.value || "midnight"; };
    select.addEventListener("change", () => {
      apply();
      try { localStorage.setItem(KEY, select.value); } catch (e) { /* storage unavailable */ }
    });
    apply();
  }

  /* ─────────────────────────────────────────────────────────────────
   *  go
   * ───────────────────────────────────────────────────────────────── */

  // Track if cookie-synced handler is already running to prevent duplicate execution
  let cookieSyncedHandling = false;

  window.addEventListener("animepahe-cookie-synced", async () => {
    if (cookieSyncedHandling) return;
    cookieSyncedHandling = true;
    try {
      const init = await api.get("/api/state");
      state.config = normalizeConfig(init.config);
      renderSettings();
    } catch (_) {}
    const banner = $("cookieBanner");
    if (banner) banner.classList.add("hidden");
    closeCfSolverModal();
    // Show green "solved" state on buttons
    highlightSolveCfButton("solved");
    // Fade to normal after a moment
    setTimeout(() => highlightSolveCfButton(false), 4000);
    toast("Cloudflare credentials updated!", "ok");
    if (state.selectedSlug) {
      loadEpisodes(state.selectedSlug, state.epPage, state.audioFilter);
    } else if (state.searchMode === "recent") {
      loadRecentReleases(state.recentPage || 1, true);
    } else if (state.searchMode === "remote" && $("searchInput").value.trim()) {
      doSearch();
    }
    cookieSyncedHandling = false;
  });

  bind();
  fillUAPresets();
  initEventSource();
  initThemePicker();
  loadInitialState();
})();
