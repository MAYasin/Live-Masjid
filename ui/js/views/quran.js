// Quran: Abdul Rahman Al-Sudais, played on the Pi speaker from the Pi's own storage.
// Named playlists (stored on the Pi, shared by everyone) plus the full list of 114 surahs.
// A surah downloads the first time it's played (and plays while downloading); "Download all"
// fetches the rest in the background. A live broadcast pauses the Quran; it continues by
// itself once the broadcast has ended.

import { api } from "../api.js";
import { $, closeModal, openModal, runAction, toast } from "../dom.js";
import { escapeHtml } from "../format.js";
import * as miniplayer from "../miniplayer.js";
import { getStatus, subscribe } from "../store.js";

let lib = null;
let playlists = [];
let visible = false;
let pollTimer;
let confirmDelete = 0;
/** Playlist being edited: { id?, surahs: number[] }. */
let draft = null;

const mb = (b) => `${(b / 1048576).toFixed(b < 10485760 ? 1 : 0)} MB`;
const fold = (s) => s.toLowerCase().normalize("NFD").replace(/[̀-ͯʿʾ]/g, "");
const surahName = (n) => lib?.surahs?.[n - 1]?.name ?? `Surah ${n}`;

function playing(s = getStatus()) {
  return s?.recording?.kind === "quran" ? s.recording : null;
}

function surahState(x) {
  switch (x.state) {
    case "ready": return `<span class="q-ready"><svg><use href="#i-check"/></svg>${mb(x.bytes)}</span>`;
    case "downloading": return x.total ? `Downloading ${Math.floor((100 * x.bytes) / x.total)}%` : `Downloading ${mb(x.bytes)}`;
    case "queued": return "Waiting to download";
    case "partial": return x.total ? `${Math.floor((100 * x.bytes) / x.total)}% downloaded` : "Partly downloaded";
    default: return "";
  }
}

function renderPlaylists() {
  const now = playing()?.playlist?.id;
  $("#pl-list").innerHTML = playlists.length
    ? playlists.map((p) => {
      const names = p.surahs.slice(0, 4).map(surahName).join(", ") + (p.surahs.length > 4 ? ` and ${p.surahs.length - 4} more` : "");
      return `<div class="item pl-row${p.id === now ? " current" : ""}">
        <div class="grow">
          <div class="item-title truncate">${escapeHtml(p.name)}${p.repeat ? ' <svg class="pl-rep" aria-label="Repeats"><use href="#i-repeat"/></svg>' : ""}</div>
          <div class="item-meta truncate">${escapeHtml(names)}</div>
        </div>
        <div class="item-actions">
          <button class="btn small ghost" data-edit="${escapeHtml(p.id)}">Edit</button>
          <button class="btn small${p.id === now ? "" : " ghost"}" data-play-pl="${escapeHtml(p.id)}"><svg><use href="#i-play"/></svg>Play</button>
        </div>
      </div>`;
    }).join("")
    : '<div class="empty small">Make a playlist of the surahs you listen to, for example Al-Fātiḥah, Al-Baqarah and Yā Sīn.</div>';
}

function renderList() {
  if (!lib) return;
  const q = fold($("#quran-filter").value.trim());
  const now = playing()?.surah;
  const rows = lib.surahs
    .filter((x) => !q || String(x.n) === q || fold(x.name).includes(q))
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

  $("#quran-store").textContent = `${lib.ready} of 114 on the Pi, ${mb(lib.stored_bytes)}`;
  const all = $("#quran-all");
  all.textContent = lib.downloading_all ? "Stop downloading" : lib.ready === 114 ? "All downloaded" : "Download all";
  all.disabled = lib.ready === 114 && !lib.downloading_all;
  $("#quran-delete").hidden = lib.stored_bytes === 0;
  $("#quran-error").hidden = !lib.error;
  $("#quran-error").textContent = lib.error || "";
}

async function load() {
  try {
    [lib, playlists] = await Promise.all([api("quran"), api("quran/playlists")]);
    renderPlaylists();
    renderList();
  } catch (err) {
    $("#quran-list").innerHTML = `<div class="empty small">${escapeHtml(err.message)}</div>`;
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

function openEditor(p) {
  draft = { id: p?.id, surahs: [...(p?.surahs ?? [])] };
  $("#pl-heading").textContent = p ? "Edit playlist" : "New playlist";
  $("#pl-name").value = p?.name ?? "";
  $("#pl-repeat").checked = Boolean(p?.repeat);
  $("#pl-delete").hidden = !p;
  if (!$("#pl-pick").options.length && lib) {
    $("#pl-pick").innerHTML = lib.surahs.map((x) => `<option value="${x.n}">${x.n}. ${escapeHtml(x.name)}</option>`).join("");
  }
  renderDraft();
  openModal("playlist-modal", "#pl-name");
}

async function saveDraft(e) {
  e.preventDefault();
  try {
    const p = await api("quran/playlists", { id: draft.id, name: $("#pl-name").value, surahs: draft.surahs, repeat: $("#pl-repeat").checked });
    closeModal("playlist-modal");
    toast(draft.id ? "Playlist saved" : `Playlist "${p.name}" created`);
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
    if (b) runAction(b, () => api("quran/play", { surah: Number(b.dataset.surah) }), "Playing on the speaker").then(() => setTimeout(load, 800));
  });
  $("#pl-list").addEventListener("click", (e) => {
    const play = e.target.closest("[data-play-pl]");
    if (play) runAction(play, () => api("quran/play", { playlist: play.dataset.playPl }), "Playing on the speaker");
    const edit = e.target.closest("[data-edit]");
    if (edit) openEditor(playlists.find((p) => p.id === edit.dataset.edit));
  });
  $("#pl-new").addEventListener("click", () => {
    if (lib) openEditor(null);
  });
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
  $("#playlist-form").addEventListener("submit", saveDraft);
  $("#pl-delete").addEventListener("click", async () => {
    if (!draft?.id) return;
    try {
      await api("quran/playlists/delete", { id: draft.id });
      closeModal("playlist-modal");
      toast("Playlist deleted");
      load();
    } catch (err) {
      toast(err.message);
    }
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
  subscribe((s) => {
    miniplayer.update($("#quran-player"), s, { quranOnly: true });
    if (!visible) return;
    // Highlight what's playing when it changes (next in the playlist).
    const now = playing(s);
    const key = `${now?.surah}/${now?.playlist?.id}`;
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
