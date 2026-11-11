// Runs inside animepahe.pw pages
(function() {
  const ext = typeof browser !== "undefined" ? browser : chrome;

  function notifySync() {
    try {
      ext.runtime.sendMessage({
        action: "animepaheLoaded",
        url: window.location.href,
        ua: navigator.userAgent
      });
    } catch (e) {
      // Extension context invalidated or not ready
    }
  }

  // 1. Notify immediately on DOM load
  notifySync();

  // 2. Cloudflare Turnstile / challenge often sets cf_clearance a few seconds after page load
  setTimeout(notifySync, 1000);
  setTimeout(notifySync, 2500);
  setTimeout(notifySync, 5000);

  // 3. Listen for visibility changes (when user switches back to this tab)
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      notifySync();
    }
  });
})();
