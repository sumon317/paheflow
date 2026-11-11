// AnimePahe GUI Auto-Sync - Popup Script
const ext = typeof browser !== "undefined" ? browser : chrome;

document.addEventListener("DOMContentLoaded", () => {
  const cookieBadge = document.getElementById("cookieBadge");
  const lastSync = document.getElementById("lastSync");
  const syncBtn = document.getElementById("syncBtn");

  function refresh() {
    ext.runtime.sendMessage({ action: "getStatus" }, (data) => {
      if (ext.runtime.lastError || !data) return;

      if (data.cfLength) {
        cookieBadge.textContent = "Detected";
        cookieBadge.className = "badge badge-ok";
      } else {
        cookieBadge.textContent = "Waiting for tab";
        cookieBadge.className = "badge badge-wait";
      }

      if (data.lastSync) {
        const date = new Date(data.lastSync);
        lastSync.textContent = date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
      } else {
        lastSync.textContent = "Not yet";
      }
    });
  }

  syncBtn.addEventListener("click", () => {
    syncBtn.textContent = "Syncing...";
    syncBtn.disabled = true;

    ext.runtime.sendMessage({ action: "syncNow" }, (res) => {
      syncBtn.disabled = false;
      syncBtn.textContent = "Sync Now";
      if (res && res.success) {
        refresh();
      } else {
        alert(res?.error || "Could not find cookie. Open animepahe.pw in a tab first!");
      }
    });
  });

  refresh();
});
