// "Masjids": searchable directory with live badges; choosing one starts following it.

import { api } from "../api.js";
import { $, $$, closeModal, openModal, setActive, toast } from "../dom.js";
import { escapeHtml, lowerFirst, timeAgo } from "../format.js";

const GENRES = ["Masjid", "Institution", "Aalim"];
let stations = [];
let filter = "all";
let searchTimer;
let navigate = () => {};
let chosen = null;

// Provinces/states and countries that add length but not meaning to a place line.
const NOISE = /^(south africa|gauteng|kwa-?zulu[- ]?natal|kzn|western cape|eastern cape|northern cape|free state|limpopo|mpumalanga|north west|rsa|united kingdom|uk|canada|usa|united states|saudi arabia|mozambique|ontario|north america)$/i;

/** "Lakewood Estate, Ormonde Ext 37 B, Ormonde, Johannesburg, Gauteng, South Africa, 2091" → "Ormonde, Johannesburg". */
export function shortPlace(location) {
  const parts = String(location || "").split(",").map((p) => p.trim()).filter((p) => p && !/^[\d\s]+$/.test(p) && !NOISE.test(p.replace(/\s+/g, " ")));
  const unique = parts.filter((p, i) => parts.findIndex((q) => q.toLowerCase() === p.toLowerCase()) === i);
  return unique.slice(-2).join(", ");
}

export function init(go) {
  navigate = go;
  $("#search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(render, 80);
  });
  $("#filters").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    filter = btn.dataset.filter;
    setActive($$("#filters button"), btn);
    render();
  });
  $("#stations").addEventListener("click", (e) => {
    const row = e.target.closest(".item[data-mount]");
    if (row) openActions(row.dataset.mount);
  });
  $("#stations").addEventListener("keydown", (e) => {
    const row = e.target.closest(".item[data-mount]");
    if (row && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); openActions(row.dataset.mount); }
  });
  $("#ms-follow").addEventListener("click", () => chosen && choose(chosen.mount, false));
  $("#ms-listen").addEventListener("click", () => chosen && choose(chosen.mount, true));
  $("#ms-recordings").addEventListener("click", () => {
    if (!chosen) return;
    closeModal("masjid-modal");
    location.hash = `recordings/${chosen.mount}`;
  });
  // "/" jumps to search, like most directory UIs.
  document.addEventListener("keydown", (e) => {
    const onMasjids = $("#view-masjids").classList.contains("active");
    if (e.key === "/" && onMasjids && !e.target.closest("input, select, textarea")) {
      e.preventDefault();
      $("#search").focus();
    }
  });
}

export async function show() {
  try {
    ({ stations } = await api("stations"));
    render();
  } catch (err) {
    $("#stations").innerHTML = `<div class="empty">${escapeHtml(err.message)}</div>`;
  }
}

function matches(s, words) {
  if (filter === "live" && !s.live) return false;
  if (filter === "heard" && !s.last_heard_ms) return false;
  if (GENRES.includes(filter) && s.genre !== filter) return false;
  const haystack = `${s.name} ${s.location} ${s.mount}`.toLowerCase();
  return words.every((w) => haystack.includes(w));
}

function compare(a, b) {
  return (b.selected - a.selected)
    || (filter === "heard" ? (b.last_heard_ms ?? 0) - (a.last_heard_ms ?? 0) : 0)
    || (b.live - a.live)
    || a.name.localeCompare(b.name);
}

function row(s) {
  const place = shortPlace(s.location) || s.genre;
  const heard = s.last_heard_ms ? `heard ${lowerFirst(timeAgo(s.last_heard_ms))}` : "";
  const meta = [s.selected ? "Following" : "", place, heard].filter(Boolean).join(" · ");
  return `<div class="item clickable" data-mount="${escapeHtml(s.mount)}" tabindex="0" role="button">
    <div class="grow">
      <div class="item-title truncate">${escapeHtml(s.name)}</div>
      <div class="item-meta truncate">${escapeHtml(meta)}</div>
    </div>
    ${s.live ? '<span class="live-tag">LIVE</span>' : ""}
    ${s.selected ? '<svg class="check"><use href="#i-check"/></svg>' : ""}
  </div>`;
}

function openActions(mount) {
  chosen = stations.find((s) => s.mount === mount);
  if (!chosen) return;
  $("#ms-name").textContent = chosen.name;
  $("#ms-place").textContent = [chosen.live ? "Live now" : "", shortPlace(chosen.location) || chosen.genre].filter(Boolean).join(" · ");
  $("#ms-follow").hidden = chosen.selected;
  $("#ms-listen").hidden = !chosen.live;
  $("#ms-listen").lastChild.textContent = chosen.selected ? "Listen now" : "Follow and listen now";
  $("#ms-following").hidden = !chosen.selected;
  openModal("masjid-modal");
}

function render() {
  const words = $("#search").value.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const list = stations.filter((s) => matches(s, words)).sort(compare);
  const liveCount = stations.filter((s) => s.live).length;
  $("#result-count").textContent = `${list.length} of ${stations.length} · ${liveCount} live now`;
  $("#stations").innerHTML = list.length ? list.map(row).join("") : '<div class="empty">No masjids match your search.</div>';
}

async function choose(mount, listen) {
  const station = stations.find((s) => s.mount === mount);
  if (!station) return;
  closeModal("masjid-modal");
  try {
    if (!station.selected) await api("select", { mount });
    for (const s of stations) s.selected = s.mount === mount;
    if (listen) await api("player/play", {});
    toast(listen ? `Playing ${station.name}` : `Following ${station.name}. It plays when they go live.`);
    navigate("now");
  } catch (err) {
    toast(err.message);
  }
}
