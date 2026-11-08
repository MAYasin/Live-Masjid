// Quran: Abdul Rahman Al-Sudais, played on the Pi speaker from the Pi's own storage.
// A surah downloads the first time it's played (and plays while downloading); "Download all"
// fetches the rest in the background. A live broadcast pauses the Quran; it continues by
// itself once the broadcast has ended.

import { api } from "../api.js";
import { $, runAction, toast } from "../dom.js";
import { escapeHtml } from "../format.js";
import { getStatus, subscribe } from "../store.js";

let lib = null;
let visible = false;
let pollTimer;
let seeking = false;
let confirmDelete = 0;

function clock(s) {
  s = Math.max(0, Math.floor(s || 0));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}

const mb = (b) => `${(b / 1048576).toFixed(b < 10485760 ? 1 : 0)} MB`;

/** The Quran currently on the speaker (from live status), or null. */
function playing(s = getStatus()) {
  return s?.recording?.kind === "quran" ? s.recording : null;
}

function surahState(x) {
  switch (x.state) {
    case "ready": return `<span class="q-ready"><svg><use href="#i-check"/></svg>${mb(x.bytes)}</span>`;
    case "downloading": return x.total ? `Downloading ${Math.floor((100 * x.bytes) / x.total)}%` : `Downloading ${mb(x.bytes)}`;
    case "queued": return "Queued";
    case "partial": return x.total ? `${Math.floor((100 * x.bytes) / x.total)}% downloaded` : "Partly downloaded";
    default: return "";
  }
}

function renderList() {
  if (!lib) return;
  const q = $("#quran-filter").value.trim().toLowerCase();
  const now = playing()?.surah;
  const rows = lib.surahs
    .filter((x) => !q || String(x.n) === q || x.name.toLowerCase().normalize("NFD").replace(/[̀-ͯ]/g, "").includes(q.normalize("NFD").replace(/[̀-ͯ]/g, "")))
    .map((x) => `
      <div class="item q-row${x.n === now ? " current" : ""}">
        <span class="q-num tabular">${x.n}</span>
        <div class="grow">
          <div class="item-title truncate">${escapeHtml(x.name)}</div>
          <div class="item-meta">${surahState(x)}</div>
        </div>
        <div class="item-actions">
          <button class="btn small${x.n === now ? "" : " ghost"}" data-surah="${x.n}"><svg><use href="#i-play"/></svg>Play</button>
        </div>
      </div>`).join("");
  $("#quran-list").innerHTML = rows || '<div class="empty small">No surah matches.</div>';

  $("#quran-store").textContent = `${lib.ready} of 114 on the Pi · ${mb(lib.stored_bytes)}`;
  const all = $("#quran-all");
  all.textContent = lib.downloading_all ? "Stop downloading" : lib.ready === 114 ? "All downloaded" : "Download all";
  all.disabled = lib.ready === 114 && !lib.downloading_all;
  $("#quran-delete").hidden = lib.stored_bytes === 0;
  $("#quran-error").hidden = !lib.error;
  $("#quran-error").textContent = lib.error || "";
}

function renderPlayer(s) {
  const r = playing(s);
  const resume = s?.quran_resume;
  const box = $("#quran-player");
  box.hidden = !r && !resume;
  if (!r && !resume) return;
  if (r) {
    $("#qp-title").textContent = r.title;
    const info = lib?.surahs?.[r.surah - 1];
    const loading = info && info.state !== "ready" && info.total ? ` · downloading ${Math.floor((100 * info.bytes) / info.total)}%` : "";
    $("#qp-status").textContent = `${r.paused ? "Paused" : "Playing on the speaker"}${loading}`;
    $("#qp-toggle").innerHTML = r.paused ? '<svg><use href="#i-play"/></svg>Play' : "Pause";
    const dur = r.duration_s || 0;
    const seek = $("#qp-seek");
    seek.max = String(Math.max(1, dur));
    seek.disabled = !dur;
    if (!seeking) seek.value = String(Math.min(r.position_s, dur || r.position_s));
    $("#qp-pos").textContent = clock(seeking ? Number(seek.value) : r.position_s);
    $("#qp-dur").textContent = dur ? clock(dur) : "–:––";
    $("#qp-prev").disabled = r.surah <= 1;
    $("#qp-next").disabled = r.surah >= 114;
    for (const el of ["#qp-controls", "#qp-seekrow"]) $(el).hidden = false;
  } else {
    $("#qp-title").textContent = resume.title;
    $("#qp-status").textContent = resume.auto
      ? `Paused for the live broadcast at ${clock(resume.position_s)}; continues when it ends`
      : `Paused at ${clock(resume.position_s)}`;
    for (const el of ["#qp-controls", "#qp-seekrow"]) $(el).hidden = true;
  }
}

async function load() {
  try {
    lib = await api("quran");
    renderList();
    renderPlayer(getStatus());
  } catch (err) {
    $("#quran-list").innerHTML = `<div class="empty small">${escapeHtml(err.message)}</div>`;
  }
}

export function init() {
  $("#quran-list").addEventListener("click", (e) => {
    const b = e.target.closest("[data-surah]");
    if (b) runAction(b, () => api("quran/play", { surah: Number(b.dataset.surah) }), "Playing on the speaker").then(() => setTimeout(load, 800));
  });
  $("#quran-filter").addEventListener("input", renderList);
  $("#quran-all").addEventListener("click", (e) =>
    runAction(e.currentTarget, () => api("quran/download", { all: !lib?.downloading_all })).then(load));
  $("#quran-delete").addEventListener("click", (e) => {
    if (Date.now() - confirmDelete > 4000) {
      confirmDelete = Date.now();
      toast("Tap again to remove the downloaded surahs from the Pi");
      return;
    }
    confirmDelete = 0;
    runAction(e.currentTarget, () => api("quran/delete", {}), (r) => `Removed ${r.removed} surahs`).then(load);
  });
  $("#qp-toggle").addEventListener("click", (e) => {
    const r = playing();
    if (r) runAction(e.currentTarget, () => api("recordings/pause", { pause: !r.paused }));
  });
  $("#qp-stop").addEventListener("click", (e) => runAction(e.currentTarget, () => api("recordings/stop", {}), "Quran stopped"));
  $("#qp-prev").addEventListener("click", (e) => runAction(e.currentTarget, () => api("quran/step", { by: -1 })));
  $("#qp-next").addEventListener("click", (e) => runAction(e.currentTarget, () => api("quran/step", { by: 1 })));
  const seek = $("#qp-seek");
  seek.addEventListener("input", () => {
    seeking = true;
    $("#qp-pos").textContent = clock(Number(seek.value));
  });
  seek.addEventListener("change", () => {
    runAction(null, () => api("quran/seek", { position_s: Number(seek.value) })).finally(() => {
      setTimeout(() => { seeking = false; }, 1500);
    });
  });
  subscribe((s) => {
    if (!visible) return;
    renderPlayer(s);
    // Highlight the current surah when it changes (next in the playlist).
    const now = playing(s)?.surah;
    if (now && !document.querySelector(`.q-row.current [data-surah="${now}"]`)) renderList();
  });
}

export function show() {
  visible = true;
  load();
  clearInterval(pollTimer);
  // Download progress: refresh while this page is open.
  pollTimer = setInterval(() => {
    if (!visible || document.hidden || !document.querySelector("#view-quran.active")) {
      visible = Boolean(document.querySelector("#view-quran.active"));
      if (!visible) clearInterval(pollTimer);
      return;
    }
    load();
  }, 3000);
}
