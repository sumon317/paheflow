// Runs inside http://127.0.0.1/* and http://localhost/* (the GUI uses a
// loopback server whose port may change).
(function() {
  const ext = typeof browser !== "undefined" ? browser : chrome;

  function readToken() {
    const meta = document.querySelector('meta[name="app-token"]');
    return meta ? meta.content : "";
  }

  function notifyGuiReload() {
    const token = readToken();
    const message = {
      action: "guiLoaded",
      url: window.location.href,
      ua: navigator.userAgent
    };
    if (token) message.token = token;

    try {
      ext.runtime.sendMessage(message);
    } catch (e) {
      // Ignore
    }
  }

  // The content script can run before <head> is parsed, so wait until the
  // document (and its meta tags) exists before reporting the session token.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", notifyGuiReload, { once: true });
  } else {
    notifyGuiReload();
  }
  window.addEventListener("load", notifyGuiReload, { once: true });

  // Listen for messages from background script when a new cookie is synced
  ext.runtime.onMessage.addListener((request) => {
    if (request.action === "cookieUpdated") {
      // Dispatch custom DOM event to notify app.js
      window.dispatchEvent(new CustomEvent("animepahe-cookie-synced", {
        detail: { length: request.length }
      }));
    }
  });

  // Also check when tab becomes visible
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      notifyGuiReload();
    }
  });
})();
