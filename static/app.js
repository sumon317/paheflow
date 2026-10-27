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
      concurrent_fragments: config.concurrent_fragments ?? 8,
      downloadDir: config.downloadDir || "",
      defaultDownloadDir: config.defaultDownloadDir || config.downloadDir || "",
      cfConfigured: config.cfConfigured ?? Boolean(cf),
      cfLength: config.cfLength ?? cf.length,
    };
  };

  /* ─────────────────────────────────────────────────────────────────
   *  state
   * ───────────────────────────────────────────────────────────────── */

  const CATALOG_PAGE_SIZE = 200;
  // Download logs arrive much faster than a user can read them. Keep the DOM
  // bounded so a long batch cannot turn every SSE event into a layout pass.
  const MAX_LOG_LINES = 1000;
  const MAX_QUEUED_LOGS = MAX_LOG_LINES * 2;
  const LIBRARY_AUTO_EXPAND_GROUPS = 5;
  const LIBRARY_AUTO_EXPAND_FILES = 200;
  const emptyOptions = () => ({ resolutions: [], audios: [], combinations: [], sourceCount: 0, episode: "", session: "" });

  let state = {
    localCatalog: [],
    remoteCatalog: [],
    catalogLimit: CATALOG_PAGE_SIZE,
    searchMode: "remote",
    selectedSlug: "",
    selectedTitle: "",
    episodes: [],
    epPage: 1,
    epLastPage: 1,
    epFilter: "",
    episodesSelected: [],
    options: emptyOptions(),
    config: { ua: "", concurrent_fragments: 8, cfConfigured: false, cfLength: 0 },
    deps: [],
    job: null,
    logSeq: 0,
    lastLogSeq: 0,
    logFilter: "all",
    autoscroll: true,
    activityHidden: false,
    lastDoneJob: "",
    libraryItems: [],
    epStatusKey: "",
    playlistKey: "",
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
      text.includes("403 challenge");
    if (!broken) return;
    const banner = $("cookieBanner");
    if (!banner) return;
    $("cookieBannerText").textContent = message;
    banner.classList.remove("hidden");
  }

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

  function renderCatalog() {
    const list = filteredCatalog();
    const shownList = list.slice(0, state.catalogLimit);
    const box = $("resultsList");
    if (!list.length) {
      const remote = state.searchMode === "remote";
      box.innerHTML = `<div class="empty"><p class="empty-title">${remote ? "No results" : "No matches in local catalog"}</p><p class="empty-text">${remote ? "Try a different search term." : "The local list holds cached slugs; hit Refresh catalog to update it."}</p></div>`;
      updateCatalogMeta(0, 0);
      return;
    }
    const hidden = list.length - shownList.length;
    box.innerHTML = shownList.map(a => `
      <div class="result ${a.slug === state.selectedSlug ? "is-selected" : ""}" data-slug="${escapeHtml(a.slug)}">
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
    `).join("") + (hidden > 0
      ? `<button class="btn btn-ghost load-more" id="loadMoreCatalog">Show ${Math.min(CATALOG_PAGE_SIZE, hidden)} more (${hidden} hidden)</button>`
      : "");
    $$$(".select-btn", box).forEach(btn => {
      btn.onclick = (e) => { e.stopPropagation(); const slug = e.currentTarget.closest(".result").dataset.slug; selectAnime(slug); };
    });
    $$$(".result", box).forEach(el => {
      el.onclick = (e) => { if (!e.target.closest(".select-btn")) selectAnime(el.dataset.slug); };
    });
    const more = $("loadMoreCatalog");
    if (more) more.onclick = () => { state.catalogLimit += CATALOG_PAGE_SIZE; renderCatalog(); };
    updateCatalogMeta(shownList.length, list.length);
  }

  function updateCatalogMeta(shown, total) {
    if (state.searchMode === "remote") {
      $("catalogMeta").textContent = total
        ? `${shown} of ${total} result(s) from animepahe.pw`
        : "No results from animepahe.pw";
    } else {
      $("catalogMeta").textContent = total
        ? `${shown} of ${total} cached slug(s) shown`
        : "No cached slugs match";
    }
  }

  function setCatalogMode(mode) {
    state.searchMode = mode;
    state.catalogLimit = CATALOG_PAGE_SIZE;
    $$("#resultModes .seg").forEach(b => b.classList.toggle("is-active", b.dataset.mode === mode));
    renderCatalog();
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
    setBusy($("refreshCatalog"), true);
    try {
      const { items } = await api.get("/api/catalog?refresh=1");
      state.localCatalog = items;
      setCatalogMode("local");
      toast("Catalog refreshed (" + items.length + " entries)", "ok");
    } catch (e) { toast("Refresh failed: " + e.message, "error"); }
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

  async function loadEpisodes(slug, page = 1) {
    if (!slug) return;
    const animeChanged = state.selectedSlug !== slug;
    state.selectedSlug = slug;
    state.epPage = page;
    state.episodes = [];
    state.epLastPage = 1;
    if (animeChanged) {
      // Episode numbers are only meaningful inside one anime. Keeping the
      // previous selection could silently download episodes of the new show.
      state.episodesSelected = [];
      state.epFilter = "";
      $("epFilter").value = "";
      state.options = emptyOptions();
      fillOptionSelects();
    }
    renderEpGrid();
    updatePagination();
    updateCommandPreview();
    setBusy($("reloadEpisodes"), true);
    try {
      const res = await api.get(`/api/episodes?slug=${encodeURIComponent(slug)}&page=${page}`);
      if (state.selectedSlug !== slug || state.epPage !== page) return;
      state.episodes = res.items;
      state.epLastPage = res.lastPage ?? 1;
      state.epPage = res.page ?? page;
      renderEpGrid();
      updatePagination();
      probeFirstEpisode();
      $("cookieBanner").classList.add("hidden");
      if (res.stale) {
        // The site refused the live request; this list came from the disk cache.
        toast(res.warning || "Showing a cached episode list — the live request failed.", "warn");
      }
    } catch (e) {
      toast("Failed to load episodes: " + e.message, "error");
      noteApiError(e.message);
    }
    finally { setBusy($("reloadEpisodes"), false); }
  }

  function updatePagination() {
    const pager = $("epPager");
    if (!pager) return;
    const pages = Math.max(1, state.epLastPage || 1);
    pager.classList.toggle("hidden", pages <= 1);
    $("epPageInfo").textContent = `Page ${state.epPage} of ${pages}`;
    $("epPrev").disabled = state.epPage <= 1;
    $("epNext").disabled = state.epPage >= pages;
  }

  function renderEpGrid() {
    const box = $("epGrid");
    const filtered = state.episodes.filter(e => {
      const hay = ("E" + epId(e.episode) + " " + epId(e.episode) + " " + (e.audio || "") + " " + (e.created_at || "")).toLowerCase();
      return hay.includes(state.epFilter.toLowerCase());
    });
    if (!filtered.length) {
      box.innerHTML = `<div class="empty"><p class="empty-title">No episodes match</p></div>`;
      return;
    }
    box.innerHTML = filtered.map((e) => {
      const id = epId(e.episode);
      const checked = state.episodesSelected.includes(id);
      return `
      <div class="ep ${checked ? "is-checked" : ""}" data-ep="${escapeHtml(id)}">
        <input type="checkbox" ${checked ? "checked" : ""} />
        <span class="ep-num">E${escapeHtml(id)}</span>
        <div class="ep-info">
          <small>${escapeHtml(e.audio || "—")}</small>
          <small>${escapeHtml(e.created_at || "")}</small>
        </div>
      </div>`;
    }).join("");
    $$(".ep", box).forEach(el => {
      const ep = el.dataset.ep;
      el.onclick = () => toggleEpisode(ep);
      el.querySelector("input").onclick = (e) => { e.stopPropagation(); toggleEpisode(ep); };
    });
    updateEpSelectionUI();
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
      const opts = await api.get(`/api/episode-options?slug=${encodeURIComponent(slug)}&episode=${encodeURIComponent(episode)}`);
      if (state.selectedSlug !== slug) return;
      state.options = opts;
      fillOptionSelects();
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
      values.map(v => `<option value="${escapeHtml(v)}">${escapeHtml(v)}p</option>`).join("");
    select.value = values.includes(selected) ? selected : "";
    return select.value;
  }

  function renderAudioOptions(resolution, selected) {
    const select = $("optAudio");
    const values = audiosForResolution(resolution);
    select.innerHTML = '<option value="">Default</option>' +
      values.map(v => `<option value="${escapeHtml(v)}">${escapeHtml(v).toUpperCase()}</option>`).join("");
    select.value = values.includes(selected) ? selected : "";
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
    const previousAudio = a.value;

    // Rebuild both lists from the probe result. Preserve the chosen audio
    // first; if its resolution is gone, fall back to Auto within that audio.
    renderResolutionOptions("", "");
    renderAudioOptions("", "");
    if (previousAudio && state.options.audios.includes(previousAudio)) {
      const keptResolution = renderResolutionOptions(previousAudio, previousResolution);
      renderAudioOptions(keptResolution, previousAudio);
    } else if (previousResolution && state.options.resolutions.includes(previousResolution)) {
      const keptAudio = renderAudioOptions(previousResolution, previousAudio);
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
    state.episodesSelected = state.episodes.map(e => epId(e.episode));
    renderEpGrid();
    updateCommandPreview();
  }
  function selectNone() {
    state.episodesSelected = [];
    renderEpGrid();
    updateCommandPreview();
  }
  function invertSelection() {
    const all = state.episodes.map(e => epId(e.episode));
    state.episodesSelected = all.filter(ep => !state.episodesSelected.includes(ep));
    renderEpGrid();
    updateCommandPreview();
  }
  function selectRange() {
    const first = prompt("First episode number:");
    if (!first) return;
    const last = prompt("Last episode number:");
    if (!last) return;
    const a = parseInt(first, 10), b = parseInt(last, 10);
    if (isNaN(a) || isNaN(b)) return;
    const lo = Math.min(a, b), hi = Math.max(a, b);
    const range = [];
    for (let n = lo; n <= hi; n++) range.push(String(n));
    state.episodesSelected = [...new Set([...state.episodesSelected, ...range])];
    renderEpGrid();
    updateCommandPreview();
  }
  function selectLatest() {
    if (!state.episodes.length) return;
    const latest = state.episodes.slice(-5).map(e => epId(e.episode));
    state.episodesSelected = [...new Set([...state.episodesSelected, ...latest])];
    renderEpGrid();
    updateCommandPreview();
  }

  function updateEpSelectionUI() {
    const count = state.episodesSelected.length;
    $("epTitle").textContent = state.selectedTitle || state.selectedSlug;
    $("epSubtitle").textContent = `${state.episodes.length} episodes • ${count} selected${count ? " — " + compressEpisodes(state.episodesSelected) : ""}`;
    $("startBtn").disabled = count === 0;
    updateActionSummary();
  }

  function compressEpisodes(arr) {
    if (!arr.length) return "";
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
    if (!state.selectedSlug) { $("commandPreview").textContent = "./animepahe-dl.sh -s <slug> -e *"; return; }
    const eps = state.episodesSelected.length ? compressEpisodes(state.episodesSelected) : "*";
    const parts = ["./animepahe-dl.sh", "-s", state.selectedSlug, "-e", eps];
    if ($("optResolution").value) parts.push("-r", $("optResolution").value);
    if ($("optAudio").value) parts.push("-o", $("optAudio").value);
    if ($("optReplace").checked) parts.push("-R");
    if ($("optListOnly").checked) parts.push("-l");
    if ($("optDebug").checked) parts.push("-d");
    $("commandPreview").textContent = parts.join(" ");
  }

  function updateActionSummary() {
    const ep = state.job?.episodes || {};
    const cur = ep.current ? "E" + ep.current : "—";
    const total = ep.requested?.length ? ep.requested.length : (ep.completed?.length || 0) + (ep.failed?.length || 0);
    $("actionAnime").textContent = state.selectedTitle || state.selectedSlug || "—";
    $("actionEpisodes").textContent = total ? `${cur} / ${total}` : "—";
    const res = ($("optResolution").value || "Auto") + "p";
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
    $("cfgFrag").value = c.concurrent_fragments ?? 8;
    $("optFragments").value = c.concurrent_fragments ?? 8;
    $("cfgDownloadDir").value = c.downloadDir || c.defaultDownloadDir || "";
    updateBadges();
    updateDownloadDirBadge();
    renderDeps();
    renderSetup();
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
    if (missing.length) {
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
      concurrent_fragments: parseInt($("cfgFrag").value || "8", 10),
      download_dir: $("cfgDownloadDir").value.trim(),
    };
    // Omitting the cookie means "keep the stored value"; the server never
    // sends it back, so an empty field must not wipe it out.
    if (cfValue) payload.cf = cfValue;
    try {
      const saved = await api.post("/api/config", payload);
      state.config = normalizeConfig(saved);
      renderSettings();
      updateSaveHint("ok", "Saved");
      toast("Configuration saved", "ok");
      if (state.config.downloadDir !== previousDir) rescanLibrary();
      return true;
    } catch (e) {
      updateSaveHint("err", "Failed: " + e.message);
      toast("Save failed: " + e.message, "error");
      return false;
    }
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
      rescanLibrary();
    } catch (e) {
      toast("Could not choose folder: " + e.message, "error");
    } finally {
      setBusy(btn, false);
    }
  }

  async function resetDownloadDir() {
    if (!confirm("Use the project folder as the destination for future downloads?")) return;
    const btn = $("resetDownloadDir");
    setBusy(btn, true);
    try {
      const saved = await api.post("/api/config", { download_dir: "" });
      state.config = normalizeConfig(saved);
      $("cfgDownloadDir").value = state.config.downloadDir;
      updateDownloadDirBadge();
      updateSaveHint("");
      toast("Download folder reset to the project default", "ok");
      rescanLibrary();
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
   *  render: library
   * ───────────────────────────────────────────────────────────────── */

  async function rescanLibrary() {
    setBusy($("rescanLibrary"), true);
    try {
      const { items } = await api.get("/api/library");
      renderLibrary(items);
    } catch (e) { toast("Library scan failed: " + e.message, "error"); }
    finally { setBusy($("rescanLibrary"), false); }
  }

  function renderLibrary(items) {
    state.libraryItems = items;
    const box = $("libraryList");
    if (!items.length) {
      box.innerHTML = `<div class="empty"><p class="empty-title">No downloaded episodes yet</p></div>`;
      $("libraryMeta").textContent = "No anime folders found";
      return;
    }
    const totalFiles = items.reduce((a, b) => a + b.count, 0);
    // Small libraries stay fully expanded. Large ones render only the series
    // headers and build episode rows on demand, so a big collection cannot
    // create tens of thousands of DOM nodes on every scan.
    const autoExpand = items.length <= LIBRARY_AUTO_EXPAND_GROUPS
      && totalFiles <= LIBRARY_AUTO_EXPAND_FILES;
    box.innerHTML = items.map((g, index) => `
      <div class="lib-group" data-index="${index}">
        <div class="lib-head">
          <span class="lib-name">${escapeHtml(g.name)}</span>
          <span class="badge">${g.count} file${g.count !== 1 ? "s" : ""} • ${escapeHtml(g.totalText)}</span>
          <div class="lib-head-actions">
            <button class="btn btn-ghost btn-sm lib-toggle" data-index="${index}" aria-expanded="${autoExpand}">${autoExpand ? "Hide episodes" : "Show episodes"}</button>
            <button class="btn btn-ghost btn-sm lib-open" data-path="${escapeHtml(g.path)}">Open</button>
            <button class="btn btn-ghost btn-sm lib-delete-folder" data-path="${escapeHtml(g.path)}" data-name="${escapeHtml(g.name)}">Delete series</button>
          </div>
        </div>
        <div class="lib-files"${autoExpand ? "" : " hidden"}></div>
      </div>
    `).join("");
    if (autoExpand) items.forEach((_, index) => renderLibraryFiles(index));
    $("libraryMeta").textContent = `${items.length} anime folder${items.length !== 1 ? "s" : ""} • ${totalFiles} episode${totalFiles !== 1 ? "s" : ""} total`;
  }

  function renderLibraryFiles(index) {
    const group = state.libraryItems[index];
    const box = $("libraryList").querySelector(`.lib-group[data-index="${index}"] .lib-files`);
    if (!group || !box || box.dataset.rendered === "1") return;
    box.innerHTML = group.episodes.map(f => `
      <div class="lib-file">
        <span class="lib-file-name">${escapeHtml(f.file)}</span>
        <span class="lib-file-size">${escapeHtml(f.sizeText)}</span>
        <span class="lib-file-date">${escapeHtml(f.modifiedText)}</span>
        <div class="lib-file-actions">
          <button class="btn btn-ghost btn-sm lib-open" data-path="${escapeHtml(f.path)}">Open</button>
          <button class="btn btn-ghost btn-sm lib-delete-file" data-path="${escapeHtml(f.path)}" data-name="${escapeHtml(f.file)}">Delete</button>
        </div>
      </div>
    `).join("");
    box.dataset.rendered = "1";
  }

  function toggleLibraryGroup(index) {
    const group = $("libraryList").querySelector(`.lib-group[data-index="${index}"]`);
    if (!group) return;
    const files = group.querySelector(".lib-files");
    const button = group.querySelector(".lib-toggle");
    const expand = files.hidden;
    if (expand) renderLibraryFiles(index);
    files.hidden = !expand;
    button.textContent = expand ? "Hide episodes" : "Show episodes";
    button.setAttribute("aria-expanded", String(expand));
  }

  async function openLibraryPath(path) {
    try {
      await api.post("/api/open", { path });
    } catch (e) {
      toast("Could not open path: " + e.message, "error");
    }
  }

  async function deleteLibraryPath(path, kind, name) {
    const prompt = kind === "folder"
      ? `Delete "${name}" and all of its episodes?`
      : `Delete "${name}"?`;
    if (!confirm(prompt)) return;
    try {
      if (kind === "folder") {
        await api.post("/api/library/delete-folder", { path });
      } else {
        await api.post("/api/library/delete", { path });
      }
      toast(kind === "folder" ? "Anime folder deleted" : "Episode deleted", "ok");
      await rescanLibrary();
    } catch (e) {
      toast("Delete failed: " + e.message, "error");
    }
  }

  function openLibraryFolder() {
    openLibraryPath(state.config.downloadDir || ".");
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
    if (!state.job || !state.job.running) {
      $("progressLabel").textContent = "Idle";
      $("progressPercent").textContent = "—";
      $("progressFill").style.width = "0%";
      $("progressEpisode").textContent = "";
      $("progressSpeed").textContent = "";
      $("progressEta").textContent = "";
      updatePill("idle");
      return;
    }
    const p = state.job.progress || {};
    if (p.percent !== null && p.percent !== undefined) {
      $("progressFill").style.width = Math.max(0, Math.min(100, p.percent)) + "%";
      $("progressPercent").textContent = p.percent.toFixed(1) + "%";
      $("progressLabel").textContent = "Downloading…";
    } else {
      $("progressFill").style.width = "0%";
      $("progressPercent").textContent = "—";
      $("progressLabel").textContent = state.job.progress?.file ? "Starting…" : "Downloading…";
    }
    $("progressEpisode").textContent = state.job.progress?.file ? "💾 " + state.job.progress.file.split("/").pop() : "";
    $("progressSpeed").textContent = state.job.progress?.speed ? "⚡ " + state.job.progress.speed : "";
    $("progressEta").textContent = state.job.progress?.eta ? "⏱ " + state.job.progress.eta : "";
    updatePill("running");
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
      case "done": txt.textContent = "Done"; break;
      case "error": txt.textContent = "Error"; break;
      default: txt.textContent = "Idle";
    }
  }

  function handleState(msg) {
    applyJob(msg);
    if (typeof msg.logSeq === "number") state.logSeq = msg.logSeq;
    if (msg.running) {
      $("startBtn").disabled = true;
      $("stopBtn").disabled = false;
      updatePill("running");
      return;
    }
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
    rescanLibrary();
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
      time.textContent = fmtTime((entry.ts || Date.now() / 1000) * 1000);
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
    const episodes = state.episodesSelected.length ? compressEpisodes(state.episodesSelected) : "*";
    const payload = {
      slug: state.selectedSlug,
      title: state.selectedTitle,
      episodes,
      resolution: $("optResolution").value.trim(),
      audio: $("optAudio").value.trim(),
      concurrent_fragments: parseInt($("optFragments").value || "8", 10),
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
      $("startBtn").disabled = true;
      $("stopBtn").disabled = false;
      updatePill("running");
    }).catch(e => toast("Start failed: " + e.message, "error"));
  }

  function stopJob() {
    api.post("/api/stop", {}).then(() => {
      updatePill("idle");
      $("startBtn").disabled = false;
      $("stopBtn").disabled = true;
    }).catch(e => toast("Stop failed: " + e.message, "error"));
  }

  /* ─────────────────────────────────────────────────────────────────
   *  tab navigation
   * ───────────────────────────────────────────────────────────────── */

  function showTab(name) {
    $$(".panel").forEach(p => p.classList.toggle("is-active", p.id === "panel-" + name));
    $$(".tab").forEach(t => t.classList.toggle("is-active", t.dataset.tab === name));
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
      if (!state.config.cfConfigured) showTab("settings");
      applyJob(init.job);
      if (init.job?.logSeq !== undefined) state.logSeq = init.job.logSeq;
      if (init.job?.running) {
        $("startBtn").disabled = true;
        $("stopBtn").disabled = false;
        updatePill("running");
      } else if (init.job?.exitCode !== null && init.job?.exitCode !== undefined) {
        updatePill(init.job.stopped ? "idle" : (init.job.exitCode === 0 ? "done" : "error"));
        $("startBtn").disabled = state.episodesSelected.length === 0;
        $("stopBtn").disabled = true;
      }
    } catch (e) {
      toast("Failed to load initial state: " + e.message, "error");
    }
    try {
      const lib = await api.get("/api/library");
      renderLibrary(lib.items);
    } catch (e) { /* the library is optional */ }
  }

  /* ─────────────────────────────────────────────────────────────────
   *  bindings
   * ───────────────────────────────────────────────────────────────── */

  function bind() {
    // tabs
    $$(".tab").forEach(btn => btn.onclick = () => showTab(btn.dataset.tab));

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
    $("reloadEpisodes").onclick = () => loadEpisodes(state.selectedSlug, 1);
    $("epPrev").onclick = () => { if (state.epPage > 1) loadEpisodes(state.selectedSlug, state.epPage - 1); };
    $("epNext").onclick = () => { if (state.epPage < state.epLastPage) loadEpisodes(state.selectedSlug, state.epPage + 1); };
    $("epFilter").addEventListener("input", debounce(() => { state.epFilter = $("epFilter").value; renderEpGrid(); }));
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
      updateOptionHints();
      updateActionSummary();
      updateCommandPreview();
    };
    $("optAudio").onchange = () => {
      // Choosing audio narrows resolutions; an unavailable resolution resets
      // to Auto so the selected audio track is preserved.
      const resolution = renderResolutionOptions($("optAudio").value, $("optResolution").value);
      renderAudioOptions(resolution, $("optAudio").value);
      updateOptionHints();
      updateActionSummary();
      updateCommandPreview();
    };
    $("optReplace").onchange = () => {
      updateActionSummary();
      updateCommandPreview();
    };
    $("optListOnly").onchange = updateCommandPreview;
    $("optDebug").onchange = updateCommandPreview;
    $("fragMinus").onclick = () => { const v = parseInt($("optFragments").value, 10); if (v > 1) { $("optFragments").value = v - 1; updateCommandPreview(); } };
    $("fragPlus").onclick = () => { const v = parseInt($("optFragments").value, 10); if (v < 64) { $("optFragments").value = v + 1; updateCommandPreview(); } };
    $("probeOptions").onclick = () => probeOptions(state.options?.episode || state.episodes[0]?.episode || 1);

    // command
    $("copyCommand").onclick = () => copyToClip($("commandPreview").textContent, "Command copied");

    // settings
    $("saveConfig").onclick = saveConfig;
    $("reloadConfig").onclick = reloadConfig;
    $("testConnection").onclick = testConnection;
    $("copyConfig").onclick = copyConfig;
    $("clearLogs").onclick = clearLogs;
    $("cfgFragMinus").onclick = () => { const v = parseInt($("cfgFrag").value, 10); if (v > 1) $("cfgFrag").value = v - 1; };
    $("cfgFragPlus").onclick = () => { const v = parseInt($("cfgFrag").value, 10); if (v < 64) $("cfgFrag").value = v + 1; };
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
    $("browseDownloadDir").onclick = browseDownloadDir;
    $("resetDownloadDir").onclick = resetDownloadDir;
    $("uaPreset").onchange = (e) => { if (e.target.value) $("cfgUa").value = e.target.value; };
    $("useBrowserUa").onclick = useBrowserUa;
    $("setupUseBrowserUa").onclick = useBrowserUa;
    $("setupFocusCookie").onclick = focusCookieField;
    $("setupSaveTest").onclick = setupSaveAndTest;

    // activity
    $("toggleActivity").onclick = () => {
      state.activityHidden = !state.activityHidden;
      const mobile = window.innerWidth <= 900;
      document.body.classList.toggle("activity-hidden", state.activityHidden && !mobile);
      document.body.classList.toggle("show-mobile-activity", !state.activityHidden && mobile);
    };
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
    $("startBtn").onclick = startJob;
    $("stopBtn").onclick = stopJob;

    // library
    $("rescanLibrary").onclick = rescanLibrary;
    $("openLibraryFolder").onclick = openLibraryFolder;
    $("libraryList").addEventListener("click", (event) => {
      const toggle = event.target.closest(".lib-toggle");
      if (toggle) { toggleLibraryGroup(Number(toggle.dataset.index)); return; }
      const open = event.target.closest(".lib-open");
      if (open) { openLibraryPath(open.dataset.path); return; }
      const file = event.target.closest(".lib-delete-file");
      if (file) { deleteLibraryPath(file.dataset.path, "file", file.dataset.name); return; }
      const folder = event.target.closest(".lib-delete-folder");
      if (folder) deleteLibraryPath(folder.dataset.path, "folder", folder.dataset.name);
    });

    // copy slug
    $("copySlug").onclick = () => { if (state.selectedSlug) copyToClip(state.selectedSlug, "Slug copied"); };

    // cookie banner
    $("bannerSettings").onclick = () => showTab("settings");
    $("bannerClose").onclick = () => $("cookieBanner").classList.add("hidden");

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

  bind();
  fillUAPresets();
  initEventSource();
  initThemePicker();
  loadInitialState();
})();
