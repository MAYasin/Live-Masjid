// "Install on your phone": a short guide for the visitor's own device, and Chrome's install
// prompt when the browser offers one (only on HTTPS; the Pi serves plain HTTP on the LAN,
// so Android adds a shortcut while iPhone's "Add to Home Screen" gives a full-screen app).

import { $, $$, openModal } from "./dom.js";

let deferred = null;

const ua = navigator.userAgent;
const ios = /iPhone|iPad|iPod/.test(ua) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const android = /Android/.test(ua);

export const installed = () =>
  window.matchMedia("(display-mode: standalone)").matches || window.navigator.standalone === true;

function steps(list) {
  return `<ol class="install-steps">${list.map((s) => `<li>${s}</li>`).join("")}</ol>`;
}

function body() {
  if (installed()) return '<p class="text-2 small">You are already using Live Masjid as an app.</p>';
  if (deferred) {
    return `<p class="text-2 small">Add Live Masjid to this device. It opens in its own window, like an app.</p>
      <button class="btn primary" id="install-now" type="button">Install</button>`;
  }
  if (ios) {
    const safari = !/CriOS|FxiOS|EdgiOS/.test(ua);
    return `${safari ? "" : '<p class="text-2 small">Open this page in <b>Safari</b> first; other browsers on iPhone can\'t add apps.</p>'}
      ${steps(["Tap the <b>Share</b> button at the bottom of Safari.", "Choose <b>Add to Home Screen</b>.", "Tap <b>Add</b>."])}
      <p class="text-3 xsmall">It opens full screen from your home screen, like an app.</p>`;
  }
  if (android) {
    return `${steps(["Tap <b>⋮</b> at the top right of Chrome.", "Tap <b>Add to Home screen</b>.", "Tap <b>Add</b>."])}
      <p class="text-3 xsmall">On your home network Chrome adds a shortcut that opens in Chrome. A full app install needs a
      secure (HTTPS) connection to the Pi, which isn't set up yet.</p>`;
  }
  return `<p class="text-2 small">Open this page on your phone (same Wi-Fi) and use this guide there. On a computer,
    bookmark it for quick access.</p>`;
}

export function open() {
  $("#install-body").innerHTML = body();
  $("#install-now")?.addEventListener("click", async () => {
    deferred.prompt();
    await deferred.userChoice.catch(() => {});
    deferred = null;
    open();
  });
  openModal("install-modal");
}

export function init() {
  window.addEventListener("beforeinstallprompt", (e) => {
    e.preventDefault();
    deferred = e;
  });
  window.addEventListener("appinstalled", () => { deferred = null; });
  for (const b of $$("[data-install]")) {
    b.hidden = installed();
    b.addEventListener("click", open);
  }
}
