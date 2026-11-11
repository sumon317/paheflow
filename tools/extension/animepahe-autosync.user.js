// ==UserScript==
// @name         AnimePahe GUI Auto-Sync
// @namespace    https://github.com/animepahe-gui
// @version      1.3.0
// @description  Automatically syncs Cloudflare cf_clearance cookie and User-Agent to your local AnimePahe GUI
// @match        *://*.animepahe.pw/*
// @match        *://*.animepahe.org/*
// @match        *://*.animepahe.ru/*
// @match        *://*.animepahe.com/*
// @match        http://127.0.0.1/*
// @match        http://localhost/*
// @grant        GM_cookie
// @grant        GM_xmlhttpRequest
// @connect      127.0.0.1
// @connect      localhost
// @run-at       document-idle
// ==/UserScript==

(function() {
    'use strict';
    const SERVER_URL = 'http://127.0.0.1:8765';
    const TOKEN_PATTERNS = [
        /<meta[^>]+name=["']app-token["'][^>]*content=["']([^"']+)["']/i,
        /<meta[^>]+content=["']([^"']+)["'][^>]*name=["']app-token["']/i
    ];

    function gmRequest(details) {
        return new Promise((resolve, reject) => {
            GM_xmlhttpRequest({
                timeout: 5000,
                ...details,
                onload: resolve,
                onerror: reject,
                ontimeout: reject
            });
        });
    }

    async function fetchToken() {
        try {
            const response = await gmRequest({ method: 'GET', url: `${SERVER_URL}/` });
            if (response.status < 200 || response.status >= 300) return '';
            const html = response.responseText || '';
            for (const pattern of TOKEN_PATTERNS) {
                const match = html.match(pattern);
                if (match) return match[1];
            }
        } catch (e) {
            console.debug('[AnimePahe Auto-Sync Userscript] GUI unreachable:', e);
        }
        return '';
    }

    function postCredentials(token, cfValue) {
        return gmRequest({
            method: 'POST',
            url: `${SERVER_URL}/api/sync-credentials`,
            headers: {
                'Content-Type': 'application/json',
                'X-App-Token': token
            },
            data: JSON.stringify({
                cf: cfValue.trim(),
                ua: navigator.userAgent
            })
        });
    }

    async function syncCookie(cfValue) {
        if (!cfValue || typeof cfValue !== 'string') return;

        try {
            // The GUI issues a new session token on every launch, so read the
            // current one from the served page instead of caching it forever.
            let token = await fetchToken();
            if (!token) return;

            let response = await postCredentials(token, cfValue);
            if (response.status === 403) {
                token = await fetchToken();
                if (!token) return;
                response = await postCredentials(token, cfValue);
            }

            console.log('[AnimePahe Auto-Sync Userscript] Synced to local GUI:', response.status);
        } catch (e) {
            console.debug('[AnimePahe Auto-Sync Userscript] Sync failed:', e);
        }
    }

    function checkAndSync() {
        if (typeof GM_cookie !== 'undefined' && GM_cookie.list) {
            GM_cookie.list({ name: 'cf_clearance' }, function(cookies, error) {
                if (!error && cookies && cookies.length > 0) {
                    syncCookie(cookies[0].value);
                }
            });
        }
    }

    // Check on load, after Cloudflare verification delays, and when tab gains focus
    checkAndSync();
    setTimeout(checkAndSync, 1500);
    setTimeout(checkAndSync, 3500);
    setTimeout(checkAndSync, 6000);

    document.addEventListener('visibilitychange', () => {
        if (document.visibilityState === 'visible') {
            checkAndSync();
        }
    });
})();
