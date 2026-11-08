// Entry point: routing, connection indicator, login, and view wiring.

import { api, setUnauthorizedHandler } from "./api.js";
import { $, $$, closeModal, initModals, openModal } from "./dom.js";
import { lowerFirst, timeAgo } from "./format.js";
import * as install from "./install.js";
import * as local from "./local.js";
import * as store from "./store.js";
import * as historyView from "./views/history.js";
import * as masjids from "./views/masjids.js";
import * as now from "./views/now.js";
import * as quran from "./views/quran.js";
import * as recordings from "./views/recordings.js";
import * as settings from "./views/settings.js";
import * as speaker from "./views/speaker.js";
import * as system from "./views/system.js";

const VIEWS = {
  now: { title: "Now playing", module: now },
  masjids: { title: "Masjids", module: masjids },
  quran: { title: "Quran", module: quran },
  speaker: { title: "Speaker", module: speaker },
  history: { title: "History", module: historyView },
  settings: { title: "Settings", module: settings },
  system: { title: "System", module: system },
  recordings: { title: "Recordings", module: recordings },
};

function go(route) {
  // Routes look like "masjids" or "recordings/lakewood" (view + optional argument).
  const [name, arg] = String(route ?? "").split("/");
  const view = VIEWS[name] ? name : "now";
  const hash = arg ? `${view}/${arg}` : view;
  for (const b of $$("[data-view]")) b.classList.toggle("active", b.dataset.view === view);
  $("#tab-more").classList.toggle("active", MORE.includes(view));
  closeSheet();
  for (const s of $$(".view")) s.classList.toggle("active", s.id === `view-${view}`);
  $("#page-title").textContent = VIEWS[view].title;
  window.scrollTo(0, 0);
  if (location.hash !== `#${hash}`) history.pushState(null, "", `#${hash}`);
  VIEWS[view].module.show(arg);
}

// Phone: pages behind "More" (and Recordings, opened from them).
const MORE = ["speaker", "history", "system", "settings", "recordings"];

function openSheet() {
  const sheet = $("#more-sheet");
  sheet.hidden = false;
  requestAnimationFrame(() => sheet.classList.add("open"));
  $("#tab-more").setAttribute("aria-expanded", "true");
  sheet.querySelector(".sheet-row")?.focus();
}

function closeSheet() {
  const sheet = $("#more-sheet");
  if (sheet.hidden) return;
  sheet.classList.remove("open");
  $("#tab-more").setAttribute("aria-expanded", "false");
  setTimeout(() => { if (!sheet.classList.contains("open")) sheet.hidden = true; }, 180);
}

function initMoreSheet() {
  $("#tab-more").addEventListener("click", () => ($("#more-sheet").hidden ? openSheet() : closeSheet()));
  $("#more-sheet").addEventListener("click", (e) => {
    if (e.target.closest("[data-sheet-close], [data-install]")) closeSheet();
  });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeSheet(); });
  // One line of current state per page, so the sheet is useful at a glance.
  store.subscribe((s) => {
    const out = s.output;
    $("#more-speaker").textContent = out.open
      ? `${out.label}${out.kind === "bluetooth" && s.bt.battery != null ? `, battery ${s.bt.battery}%` : ""}`
      : s.bt.needs_repair ? "Needs re-pairing" : "Not connected";
    $("#more-history").textContent = s.last_heard_ms ? `Last heard ${lowerFirst(timeAgo(s.last_heard_ms))}` : "No broadcasts yet";
    $("#more-system").textContent = `Version ${s.version}`;
    $("#more-settings").textContent = `Auto-play ${s.autoplay ? "on" : "off"}${s.password_set ? ", password set" : ""}`;
  });
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
local.init();
initNavigation();
initMoreSheet();
install.init();
initConnectionIndicator();
initLogin();
boot();
