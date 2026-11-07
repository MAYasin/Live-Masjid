// Entry point: routing, connection indicator, login, and view wiring.

import { api, setUnauthorizedHandler } from "./api.js";
import { $, $$, closeModal, initModals, openModal } from "./dom.js";
import * as store from "./store.js";
import * as historyView from "./views/history.js";
import * as masjids from "./views/masjids.js";
import * as now from "./views/now.js";
import * as settings from "./views/settings.js";
import * as speaker from "./views/speaker.js";
import * as system from "./views/system.js";

const VIEWS = {
  now: { title: "Now playing", module: now },
  masjids: { title: "Masjids", module: masjids },
  speaker: { title: "Speaker", module: speaker },
  history: { title: "History", module: historyView },
  settings: { title: "Settings", module: settings },
  system: { title: "System", module: system },
};

function go(name) {
  const view = VIEWS[name] ? name : "now";
  for (const b of $$("[data-view]")) b.classList.toggle("active", b.dataset.view === view);
  for (const s of $$(".view")) s.classList.toggle("active", s.id === `view-${view}`);
  $("#page-title").textContent = VIEWS[view].title;
  window.scrollTo(0, 0);
  if (location.hash !== `#${view}`) history.pushState(null, "", `#${view}`);
  VIEWS[view].module.show();
}

function initNavigation() {
  document.addEventListener("click", (e) => {
    const target = e.target.closest("[data-view], [data-go]");
    if (target) go(target.dataset.view ?? target.dataset.go);
  });
  window.addEventListener("hashchange", () => go(location.hash.slice(1)));
}

function initConnectionIndicator() {
  store.onConnection((connected) => {
    $("#conn-dot").className = `dot ${connected ? "ok" : "err"}`;
    $("#conn-text").textContent = connected ? "Connected" : "Offline";
    $("#offline").hidden = connected;
  });
  store.subscribe((s) => { $("#version").textContent = `Version ${s.version}`; });
}

function initLogin() {
  setUnauthorizedHandler(() => openModal("login-modal", "#login-password"));
  $("#login-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    $("#login-error").textContent = "";
    try {
      await api("login", { password: $("#login-password").value });
      closeModal("login-modal");
      boot();
    } catch (err) {
      $("#login-error").textContent = err.message;
    }
  });
}

let started = false;

async function boot() {
  try {
    await store.start();
  } catch {
    return; // login modal or offline notice is already showing
  }
  if (!started) {
    started = true;
    for (const { module } of Object.values(VIEWS)) module.init(go);
  }
  go(location.hash.slice(1));
}

initModals();
initNavigation();
initConnectionIndicator();
initLogin();
boot();
