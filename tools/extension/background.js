// AnimePahe GUI Auto-Sync - Background Script
const ext = typeof browser !== "undefined" ? browser : chrome;
const DEFAULT_SERVER_URL = "http://127.0.0.1:8765";

let lastSyncedCookie = "";
let lastSyncedTime = 0;

async function getServerUrl() {
  try {
    const data = await ext.storage.local.get("serverUrl");
    return data.serverUrl || DEFAULT_SERVER_URL;
  } catch (e) {
    return DEFAULT_SERVER_URL;
  }
}

// The GUI injects a per-launch session token into its index page. Every API
// request must carry it, so the extension reads it from that page and caches
// it until the GUI restarts with a new token.
const TOKEN_PATTERNS = [
  /<meta[^>]+name=["']app-token["'][^>]*content=["']([^"']+)["']/i,
  /<meta[^>]+content=["']([^"']+)["'][^>]*name=["']app-token["']/i
];

function extractAppToken(html) {
  for (const pattern of TOKEN_PATTERNS) {
    const match = html.match(pattern);
    if (match) return match[1];
  }
  return "";
}

async function fetchAppToken(baseUrl) {
  try {
    const res = await fetch(`${baseUrl}/`, { cache: "no-store" });
    if (!res.ok) return "";
    return extractAppToken(await res.text());
  } catch (e) {
    return "";
  }
}

async function getAppToken(baseUrl, force = false) {
  if (!force) {
    try {
      const data = await ext.storage.local.get(["appToken", "appTokenBase"]);
      if (data.appToken && data.appTokenBase === baseUrl) {
        return data.appToken;
      }
    } catch (e) {}
  }

  const token = await fetchAppToken(baseUrl);
  if (token) {
    try {
      await ext.storage.local.set({ appToken: token, appTokenBase: baseUrl });
    } catch (e) {}
  }
  return token;
}

async function postCredentials(url, token, cfValue, uaValue) {
  return fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-App-Token": token
    },
    body: JSON.stringify({
      cf: cfValue.trim(),
      ua: uaValue || navigator.userAgent
    })
  });
}

// Push cookie to the local Rust GUI
async function syncToGui(cfValue, uaValue, force = false) {
  if (!cfValue || typeof cfValue !== "string") {
    return { success: false, error: "Empty cookie" };
  }

  // Avoid spamming the server with the exact same cookie within 3 seconds unless forced
  const now = Date.now();
  if (!force && cfValue === lastSyncedCookie && now - lastSyncedTime < 3000) {
    return { success: true, message: "Already synced" };
  }

  const baseUrl = await getServerUrl();
  const url = `${baseUrl}/api/sync-credentials`;

  try {
    let token = await getAppToken(baseUrl);
    if (!token) {
      return {
        success: false,
        error: "Could not read the GUI session token. Start the AnimePahe GUI and try again."
      };
    }

    let res = await postCredentials(url, token, cfValue, uaValue);

    // The GUI issues a fresh token on every launch. Refresh once and retry so
    // a restarted GUI heals itself without requiring a manual sync.
    if (res.status === 403) {
      token = await getAppToken(baseUrl, true);
      if (token) {
        res = await postCredentials(url, token, cfValue, uaValue);
      }
    }

    if (res.ok) {
      lastSyncedCookie = cfValue;
      lastSyncedTime = now;
      console.log("[AnimePahe Auto-Sync] Successfully synced cf_clearance to local GUI! Length:", cfValue.length);

      await ext.storage.local.set({
        lastSync: now,
        lastStatus: "Synced",
        cfLength: cfValue.length
      });

      // Notify any open GUI tabs
      try {
        const tabs = await ext.tabs.query({ url: ["http://127.0.0.1:*/*", "http://localhost:*/*"] });
        for (const tab of tabs) {
          ext.tabs.sendMessage(tab.id, { action: "cookieUpdated", length: cfValue.length }).catch(() => {});
        }
      } catch (e) {}

      return { success: true };
    } else {
      const errText = await res.text();
      console.warn("[AnimePahe Auto-Sync] Server error:", errText);
      return { success: false, error: errText };
    }
  } catch (err) {
    console.debug("[AnimePahe Auto-Sync] Local GUI unreachable:", err.message);
    return { success: false, error: err.message };
  }
}

// Find the latest cf_clearance cookie across all domains & partitions
async function findLatestCookie() {
  // Method 1: Query all cookies named "cf_clearance"
  try {
    const allCookies = await ext.cookies.getAll({ name: "cf_clearance" });
    const matching = allCookies.filter(c =>
      c.domain.includes("animepahe") || c.domain.includes("kwik")
    );

    if (matching.length > 0) {
      // Sort to find the newest or longest-lived cookie
      matching.sort((a, b) => (b.expirationDate || 0) - (a.expirationDate || 0));
      if (matching[0] && matching[0].value) {
        return matching[0].value;
      }
    }
  } catch (e) {
    console.debug("[AnimePahe Auto-Sync] cookies.getAll failed:", e);
  }

  // Method 2: Direct lookup by common URLs
  const candidateUrls = [
    "https://animepahe.pw/",
    "https://animepahe.pw",
    "https://animepahe.org/",
    "https://animepahe.ru/",
    "https://kwik.cx/"
  ];

  for (const u of candidateUrls) {
    try {
      const c = await ext.cookies.get({ url: u, name: "cf_clearance" });
      if (c && c.value) {
        return c.value;
      }
    } catch (e) {}
  }

  return null;
}

// Automatically reload an existing AnimePahe tab or open a new background tab
async function refreshOrOpenAnimePahe() {
  try {
    const tabs = await ext.tabs.query({
      url: [
        "*://*.animepahe.pw/*",
        "*://*.animepahe.org/*",
        "*://*.animepahe.ru/*",
        "*://*.animepahe.com/*"
      ]
    });

    if (tabs.length > 0) {
      console.log("[AnimePahe Auto-Sync] Reloading existing AnimePahe tab(s)...");
      for (const tab of tabs) {
        ext.tabs.reload(tab.id);
      }
    } else {
      console.log("[AnimePahe Auto-Sync] Opening background AnimePahe tab to obtain fresh cookie...");
      ext.tabs.create({ url: "https://animepahe.pw/", active: false });
    }
  } catch (e) {
    console.debug("[AnimePahe Auto-Sync] Could not reload AnimePahe tab:", e);
  }
}

// Master check and sync function
async function checkAndSync(customUa, force = false) {
  const cookieValue = await findLatestCookie();
  if (cookieValue) {
    return await syncToGui(cookieValue, customUa || navigator.userAgent, force);
  }
  return { success: false, error: "No cf_clearance cookie found. Open animepahe.pw to obtain one." };
}

// 1. Listen for real-time cookie creation/updates
ext.cookies.onChanged.addListener((changeInfo) => {
  const c = changeInfo.cookie;
  if (
    c.name === "cf_clearance" &&
    (c.domain.includes("animepahe") || c.domain.includes("kwik")) &&
    !changeInfo.removed &&
    c.value
  ) {
    console.log("[AnimePahe Auto-Sync] Cookie change detected for domain:", c.domain);
    syncToGui(c.value, navigator.userAgent, true);
  }
});

// 2. Listen for messages from content scripts (animepahe tab or GUI tab refreshed)
ext.runtime.onMessage.addListener((request, sender, sendResponse) => {
  if (request.action === "animepaheLoaded") {
    console.log("[AnimePahe Auto-Sync] AnimePahe page refreshed/loaded, checking cookie...");
    checkAndSync(request.ua, true).then(sendResponse);
    return true;
  }
  if (request.action === "guiLoaded") {
    console.log("[AnimePahe Auto-Sync] GUI page opened/refreshed. Triggering fresh AnimePahe check...");
    if (typeof request.token === "string" && request.token) {
      let baseUrl = DEFAULT_SERVER_URL;
      try {
        baseUrl = new URL(request.url).origin;
      } catch (e) {}
      try {
        const pending = ext.storage.local.set({
          appToken: request.token,
          appTokenBase: baseUrl,
          serverUrl: baseUrl
        });
        if (pending && typeof pending.catch === "function") pending.catch(() => {});
      } catch (e) {}
    }
    refreshOrOpenAnimePahe();
    checkAndSync(request.ua, false).then(sendResponse);
    return true;
  }
  if (request.action === "syncNow") {
    checkAndSync(null, true).then(sendResponse);
    return true;
  }
  if (request.action === "getStatus") {
    ext.storage.local.get(["lastSync", "lastStatus", "cfLength"]).then(sendResponse);
    return true;
  }
});

// 3. Listen for tab updates (URL change, reload complete)
ext.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (tab.url && (tab.url.includes("animepahe.") || tab.url.includes(":8765"))) {
    checkAndSync(navigator.userAgent, false);
  }
});

// 4. Listen for tab switching
ext.tabs.onActivated.addListener(async (activeInfo) => {
  try {
    const tab = await ext.tabs.get(activeInfo.tabId);
    if (tab.url && (tab.url.includes("animepahe.") || tab.url.includes(":8765"))) {
      checkAndSync(navigator.userAgent, false);
    }
  } catch (e) {}
});

// 5. Periodic check every 15 seconds
setInterval(() => {
  checkAndSync(navigator.userAgent, false);
}, 15000);

// Run initial check
checkAndSync(navigator.userAgent, true);
