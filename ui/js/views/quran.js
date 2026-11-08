// Quran on the Pi speaker: any reciter from mp3quran.net, named playlists (shared, optionally
// with their own reciter and a weekly schedule) and the full list of 114 surahs. A surah
// downloads the first time it's played (and plays while downloading); "Download all" fetches the
// rest of the chosen recitation. A live broadcast pauses the Quran; it continues by itself once
// the broadcast has ended.

import { api } from "../api.js";
import { $, closeModal, openModal, runAction, toast } from "../dom.js";
import { escapeHtml } from "../format.js";
import * as miniplayer from "../miniplayer.js";
import { getStatus, subscribe } from "../store.js";

const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAY_NAMES = ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"];

let lib = null;
let playlists = [];
let reciters = null; // { default, reciters: [...] }
let visible = false;
let pollTimer;
let confirmDelete = 0;
/** Playlist being edited: { id?, surahs: number[], reciter: string|null }. */
let draft = null;
/** What the reciter picker is choosing for: "default" or "playlist". */
let pickerFor = "default";

const mb = (b) => `${(b / 1048576).toFixed(b < 10485760 ? 1 : 0)} MB`;
const fold = (s) => s.toLowerCase().normalize("NFD").replace(/[̀-ͯʿʾ]/g, "");
const surahName = (n) => lib?.surahs?.[n - 1]?.name ?? `Surah ${n}`;
const reciterOf = (key) => reciters?.reciters.find((r) => r.key === key);
const reciterLabel = (r) => (r ? (r.style ? `${r.name} (${r.style})` : r.name) : "");

function playing(s = getStatus()) {
  return s?.recording?.kind === "quran" ? s.recording : null;
}

/** "Fridays at 11:00", "Every day at 05:30", "Mon, Thu at 20:00". */
function scheduleText(p) {
  if (!p.time || !p.days?.length) return "";
  const d = p.days;
  const when = d.length === 7 ? "Every day"
    : d.length === 5 && [1, 2, 3, 4, 5].every((x) => d.includes(x)) ? "Weekdays"
    : d.length === 1 ? DAY_NAMES[d[0]]
    : d.map((x) => DAYS[x]).join(", ");
  return `${when} at ${p.time}`;
}

function surahState(x) {
  switch (x.state) {
    case "ready": return `<span class="q-ready"><svg><use href="#i-check"/></svg>${mb(x.bytes)}</span>`;
    case "downloading": return x.total ? `Downloading ${Math.floor((100 * x.bytes) / x.total)}%` : `Downloading ${mb(x.bytes)}`;
    case "queued": return "Waiting to download";
    case "partial": return x.total ? `${Math.floor((100 * x.bytes) / x.total)}% downloaded` : "Partly downloaded";
    case "unavailable": return "Not recited by this reciter";
    default: return "";
  }
}

function renderPlaylists() {
  const now = playing()?.playlist?.id;
  $("#pl-list").innerHTML = playlists.length
    ? playlists.map((p) => {
      const names = p.surahs.slice(0, 3).map(surahName).join(", ") + (p.surahs.length > 3 ? ` +${p.surahs.length - 3}` : "");
      const meta = [scheduleText(p), p.reciter ? reciterOf(p.reciter)?.name : "", names].filter(Boolean).join(" · ");
      return `<div class="item pl-row${p.id === now ? " current" : ""}">
        <div class="grow">
          <div class="item-title truncate">${escapeHtml(p.name)}${p.repeat ? ' <svg class="pl-rep" aria-label="Repeats"><use href="#i-repeat"/></svg>' : ""}</div>
          <div class="item-meta truncate">${escapeHtml(meta)}</div>
        </div>
        <div class="item-actions">
          <button class="btn small ghost" data-edit="${escapeHtml(p.id)}">Edit</button>
          <button class="btn small${p.id === now ? "" : " ghost"}" data-play-pl="${escapeHtml(p.id)}"><svg><use href="#i-play"/></svg>Play</button>
        </div>
      </div>`;
    }).join("")
    : '<div class="empty small">Make a playlist of the surahs you listen to, for example Al-Fātiḥah, Al-Baqarah and Yā Sīn. It can play by itself on chosen days.</div>';
}

function renderList() {
  if (!lib) return;
  const q = fold($("#quran-filter").value.trim());
  const now = playing();
  const mine = now?.reciter_key === lib.reciter.key ? now?.surah : null;
  const rows = lib.surahs
    .filter((x) => !q || String(x.n) === q || fold(x.name).includes(q))
    .map((x) => {
      const off = x.state === "unavailable";
      return `<div class="item q-row${x.n === mine ? " current" : ""}${off ? " unavailable" : ""}">
        <span class="q-num tabular">${x.n}</span>
        <div class="grow">
          <div class="item-title truncate">${escapeHtml(x.name)}</div>
          <div class="item-meta">${surahState(x)}</div>
        </div>
        <div class="item-actions">
          ${off ? "" : `<button class="btn small${x.n === mine ? "" : " ghost"}" data-surah="${x.n}"><svg><use href="#i-play"/></svg>Play</button>`}
        </div>
      </div>`;
    }).join("");
  $("#quran-list").innerHTML = rows || '<div class="empty small">No surah matches.</div>';

  $("#reciter-name").textContent = lib.reciter.name;
  $("#quran-store").textContent = `${lib.ready} of ${lib.available} surahs on the Pi for this reciter, ${mb(lib.stored_bytes)} in total`;
  const all = $("#quran-all");
  all.textContent = lib.downloading_all ? "Stop downloading" : lib.ready >= lib.available ? "All downloaded" : "Download all";
  all.disabled = lib.ready >= lib.available && !lib.downloading_all;
  $("#quran-delete").hidden = lib.stored_bytes === 0;
  $("#quran-error").hidden = !lib.error;
  $("#quran-error").textContent = lib.error || "";
}

async function load() {
  try {
    [lib, playlists] = await Promise.all([api("quran"), api("quran/playlists")]);
    reciters ??= await api("quran/reciters").catch(() => null);
    renderPlaylists();
    renderList();
  } catch (err) {
    $("#quran-list").innerHTML = `<div class="empty small">${escapeHtml(err.message)}</div>`;
  }
}

// ------------------------------------------------------------------ reciter picker

function renderReciters() {
  if (!reciters) return;
  const q = fold($("#reciter-search").value.trim());
  const current = pickerFor === "playlist" ? draft?.reciter ?? "" : reciters.default;
  const opt = (r) => `<button type="button" class="reciter-opt" role="option" data-key="${escapeHtml(r.key)}" aria-selected="${r.key === current}">
      <span class="grow"><span class="truncate">${escapeHtml(r.name)}</span>${r.style || r.surahs.length < 114 ? `<small>${escapeHtml([r.style, r.surahs.length < 114 ? `${r.surahs.length} surahs` : ""].filter(Boolean).join(", "))}</small>` : ""}</span>
      <svg><use href="#i-check"/></svg></button>`;
  const list = reciters.reciters.filter((r) => !q || fold(`${r.name} ${r.style}`).includes(q));
  const popular = list.filter((r) => r.popular);
  const rest = list.filter((r) => !r.popular);
  let html = "";
  if (pickerFor === "playlist" && !q) {
    const def = reciterOf(reciters.default);
    html += `<button type="button" class="reciter-opt" role="option" data-key="" aria-selected="${current === ""}">
      <span class="grow">App default<small>${escapeHtml(reciterLabel(def))}</small></span><svg><use href="#i-check"/></svg></button>`;
  }
  if (popular.length) html += `<div class="group-label">Popular</div>${popular.map(opt).join("")}`;
  if (rest.length) html += `<div class="group-label">All reciters</div>${rest.map(opt).join("")}`;
  $("#reciter-list").innerHTML = html || '<div class="empty small">No reciter matches.</div>';
}

async function openPicker(target) {
  pickerFor = target;
  reciters ??= await api("quran/reciters").catch(() => null);
  if (!reciters) return toast("The reciter list isn't available yet");
  $("#reciter-heading").textContent = target === "playlist" ? "Reciter for this playlist" : "Reciter";
  $("#reciter-search").value = "";
  renderReciters();
  openModal("reciter-modal", "#reciter-search");
}

async function pick(key) {
  if (pickerFor === "playlist") {
    draft.reciter = key || null;
    paintDraftReciter();
    closeModal("reciter-modal");
    return;
  }
  try {
    await api("quran/reciters", { reciter: key });
    reciters.default = key;
    closeModal("reciter-modal");
    toast(`Reciter: ${reciterOf(key)?.name ?? key}`);
    load();
  } catch (err) {
    toast(err.message);
  }
}

// ------------------------------------------------------------------ playlist editor

function renderDraft() {
  $("#pl-chosen").innerHTML = draft.surahs.length
    ? draft.surahs.map((n, i) => `<div class="pl-item">
        <span class="q-num tabular">${n}</span><span class="grow truncate">${escapeHtml(surahName(n))}</span>
        <button type="button" class="icon-btn sm" data-move="${i}" data-by="-1" aria-label="Move up" ${i === 0 ? "disabled" : ""}><svg><use href="#i-up"/></svg></button>
        <button type="button" class="icon-btn sm" data-move="${i}" data-by="1" aria-label="Move down" ${i === draft.surahs.length - 1 ? "disabled" : ""}><svg><use href="#i-down"/></svg></button>
        <button type="button" class="icon-btn sm" data-remove="${i}" aria-label="Remove"><svg><use href="#i-x"/></svg></button>
      </div>`).join("")
    : '<div class="empty small">No surahs yet. Add them below, in the order you want.</div>';
}

function paintDraftReciter() {
  const r = draft.reciter ? reciterOf(draft.reciter) : null;
  $("#pl-reciter-name").textContent = r ? reciterLabel(r) : `App default (${reciterOf(reciters?.default)?.name ?? "Al-Sudais"})`;
}

function paintSchedule() {
  $("#pl-schedule").hidden = !$("#pl-scheduled").checked;
}

function openEditor(p) {
  draft = { id: p?.id, surahs: [...(p?.surahs ?? [])], reciter: p?.reciter ?? null };
  $("#pl-heading").textContent = p ? "Edit playlist" : "New playlist";
  $("#pl-name").value = p?.name ?? "";
  $("#pl-repeat").checked = Boolean(p?.repeat);
  $("#pl-delete").hidden = !p;
  $("#pl-scheduled").checked = Boolean(p?.time);
  $("#pl-time").value = p?.time ?? "11:00";
  for (const b of document.querySelectorAll("#pl-days [data-day]")) {
    b.setAttribute("aria-pressed", String(Boolean(p?.days?.includes(Number(b.dataset.day)))));
  }
  if (!$("#pl-pick").options.length && lib) {
    $("#pl-pick").innerHTML = lib.surahs.map((x) => `<option value="${x.n}">${x.n}. ${escapeHtml(x.name)}</option>`).join("");
  }
  paintDraftReciter();
  paintSchedule();
  renderDraft();
  openModal("playlist-modal", "#pl-name");
}

async function saveDraft(e) {
  e.preventDefault();
  const scheduled = $("#pl-scheduled").checked;
  const days = [...document.querySelectorAll('#pl-days [aria-pressed="true"]')].map((b) => Number(b.dataset.day));
  try {
    const p = await api("quran/playlists", {
      id: draft.id,
      name: $("#pl-name").value,
      surahs: draft.surahs,
      repeat: $("#pl-repeat").checked,
      reciter: draft.reciter,
      days: scheduled ? days : [],
      time: scheduled ? $("#pl-time").value : null,
    });
    closeModal("playlist-modal");
    toast(scheduleText(p) ? `Saved. Plays ${scheduleText(p).replace(/^./, (c) => c.toLowerCase())}` : draft.id ? "Playlist saved" : `Playlist "${p.name}" created`);
    load();
  } catch (err) {
    toast(err.message);
  }
}

// ------------------------------------------------------------------ wiring

export function init() {
  miniplayer.mount($("#quran-player"));
  $("#quran-list").addEventListener("click", (e) => {
    const b = e.target.closest("[data-surah]");
    if (b) runAction(b, () => api("quran/play", { surah: Number(b.dataset.surah), reciter: lib?.reciter.key }), "Playing on the speaker").then(() => setTimeout(load, 800));
  });
  $("#pl-list").addEventListener("click", (e) => {
    const play = e.target.closest("[data-play-pl]");
    if (play) runAction(play, () => api("quran/play", { playlist: play.dataset.playPl }), "Playing on the speaker");
    const edit = e.target.closest("[data-edit]");
    if (edit) openEditor(playlists.find((p) => p.id === edit.dataset.edit));
  });
  $("#pl-new").addEventListener("click", () => { if (lib) openEditor(null); });
  $("#pl-add").addEventListener("click", () => {
    draft.surahs.push(Number($("#pl-pick").value));
    renderDraft();
    $("#pl-chosen").lastElementChild?.scrollIntoView({ block: "nearest" });
  });
  $("#pl-chosen").addEventListener("click", (e) => {
    const mv = e.target.closest("[data-move]");
    if (mv) {
      const i = Number(mv.dataset.move), j = i + Number(mv.dataset.by);
      [draft.surahs[i], draft.surahs[j]] = [draft.surahs[j], draft.surahs[i]];
      renderDraft();
    }
    const rm = e.target.closest("[data-remove]");
    if (rm) {
      draft.surahs.splice(Number(rm.dataset.remove), 1);
      renderDraft();
    }
  });
  $("#pl-reciter").addEventListener("click", () => openPicker("playlist"));
  $("#pl-scheduled").addEventListener("change", paintSchedule);
  $("#pl-days").addEventListener("click", (e) => {
    const b = e.target.closest("[data-day]");
    if (b) b.setAttribute("aria-pressed", String(b.getAttribute("aria-pressed") !== "true"));
  });
  $("#playlist-form").addEventListener("submit", saveDraft);
  $("#pl-delete").addEventListener("click", async () => {
    if (!draft?.id) return;
    try {
      await api("quran/playlists/remove", { id: draft.id });
      closeModal("playlist-modal");
      toast("Playlist deleted");
      load();
    } catch (err) {
      toast(err.message);
    }
  });
  $("#reciter-chip").addEventListener("click", () => openPicker("default"));
  $("#reciter-search").addEventListener("input", renderReciters);
  $("#reciter-list").addEventListener("click", (e) => {
    const o = e.target.closest("[data-key]");
    if (o) pick(o.dataset.key);
  });
  $("#quran-filter").addEventListener("input", renderList);
  $("#quran-all").addEventListener("click", (e) =>
    runAction(e.currentTarget, () => api("quran/download", { all: !lib?.downloading_all, reciter: lib?.reciter.key })).then(load));
  $("#quran-delete").addEventListener("click", (e) => {
    if (Date.now() - confirmDelete > 4000) {
      confirmDelete = Date.now();
      toast("Tap again to remove every downloaded surah from the Pi");
      return;
    }
    confirmDelete = 0;
    runAction(e.currentTarget, () => api("quran/delete", {}), (r) => `Removed ${r.removed} surahs`).then(load);
  });
  subscribe((s) => {
    miniplayer.update($("#quran-player"), s, { quranOnly: true, volume: true });
    if (!visible) return;
    // Highlight what's playing when it changes (next in the playlist).
    const now = playing(s);
    const key = `${now?.reciter_key}/${now?.surah}/${now?.playlist?.id}`;
    if (key !== init.lastKey) {
      init.lastKey = key;
      renderList();
      renderPlaylists();
    }
  });
}

export function show() {
  visible = true;
  load();
  clearInterval(pollTimer);
  // Download progress: refresh while this page is open.
  pollTimer = setInterval(() => {
    visible = Boolean(document.querySelector("#view-quran.active"));
    if (!visible) return clearInterval(pollTimer);
    if (!document.hidden) load();
  }, 3000);
}
