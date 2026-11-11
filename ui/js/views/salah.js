// Salah times card, used on the home screen and on each masjid's page (one `SalahCard` per
// card). Data comes from the Pi's cache: masjidboardlive.com (detailed) or masjidscreen.co.za
// (fallback). Shows today's Gregorian + Hijri date, the five salah with adhān/iqāmah, the next one
// with a countdown, the other times of the day, Jumu'ah and upcoming changes, compactly.
// "Change" picks the source per masjid (one shared dialog).

import { api } from "../api.js";
import { emptyState } from "../components.js";
import { $, closeModal, openModal, toast } from "../dom.js";
import { escapeHtml } from "../format.js";

const HIJRI_MONTHS = ["Muharram", "Safar", "Rabīʿ al-Awwal", "Rabīʿ al-Thānī", "Jumādā al-Ūlā", "Jumādā al-Ākhirah",
  "Rajab", "Shaʿbān", "Ramaḍān", "Shawwāl", "Dhū al-Qaʿdah", "Dhū al-Ḥijjah"];

/** The card whose source the "Change" dialog is editing. */
let editing = null;
const cards = [];

function at(hhmm, dayOffset = 0) {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date();
  d.setDate(d.getDate() + dayOffset);
  d.setHours(h, m, 0, 0);
  return d;
}

function inWords(ms) {
  const mins = Math.max(0, Math.round(ms / 60000));
  if (mins < 1) return "now";
  if (mins < 60) return `in ${mins} min`;
  return `in ${Math.floor(mins / 60)} h ${String(mins % 60).padStart(2, "0")} min`;
}

/** Today's salah list, with Jumu'ah replacing Zuhr on Fridays. */
function todays(d) {
  const friday = new Date().getDay() === 5;
  return d.times.map((t) => (t.key === "zuhr" && friday && d.jummah ? { ...d.jummah, key: "zuhr" } : t));
}

/** The next athaan or jamaat after now: { index, kind, when } (or Fajr tomorrow). */
function nextEvent(list) {
  const now = Date.now();
  for (let i = 0; i < list.length; i++) {
    for (const kind of ["athaan", "jamaat"]) {
      const t = list[i][kind];
      if (t && at(t).getTime() > now) return { index: i, kind, when: at(t) };
    }
  }
  const fajr = list[0];
  const t = fajr && (fajr.athaan || fajr.jamaat);
  return t ? { index: 0, kind: fajr.athaan ? "athaan" : "jamaat", when: at(t, 1), tomorrow: true } : null;
}

/** Hijri date (Umm al-Qura, shifted by the masjid's adjustment). The Islamic day begins at Maghrib. */
function hijri(d) {
  const maghrib = d.times.find((t) => t.key === "maghrib");
  const sunset = d.day?.find((m) => m.key === "sunset")?.time || maghrib?.athaan;
  const date = new Date();
  const afterMaghrib = sunset && Date.now() >= at(sunset).getTime();
  date.setDate(date.getDate() + (d.hijri_adjust || 0) + (afterMaghrib ? 1 : 0));
  for (const cal of ["islamic-umalqura", "islamic-civil", "islamic"]) {
    try {
      const parts = new Intl.DateTimeFormat(`en-u-ca-${cal}-nu-latn`, { day: "numeric", month: "numeric", year: "numeric" }).formatToParts(date);
      const get = (t) => Number(parts.find((p) => p.type === t)?.value);
      const [day, month, year] = [get("day"), get("month"), parseInt(parts.find((p) => p.type === "year")?.value, 10)];
      if (day && month >= 1 && month <= 12 && year) return `${day} ${HIJRI_MONTHS[month - 1]} ${year} AH`;
    } catch { /* calendar not supported here */ }
  }
  return "";
}

/** Other times of the day as compact label/time pairs (Zawāl as a range). */
function dayTimes(d) {
  const day = d.day || [];
  const get = (k) => day.find((m) => m.key === k)?.time;
  const out = [];
  for (const m of day) {
    if (m.key === "istiwaCaution") {
      const end = get("zawaalEnd");
      out.push(["Zawāl", end ? `${m.time}–${end}` : m.time]);
    } else if (m.key === "zawaalEnd") {
      if (!get("istiwaCaution")) out.push(["Zawāl ends", m.time]);
    } else if (m.key === "fajrStarts" && get("sehriEnds") === m.time) {
      // Same moment as "Suhūr ends": shown once.
    } else if (m.key === "sehriEnds" && get("fajrStarts") === m.time) {
      out.push(["Suhūr ends · Fajr", m.time]);
    } else {
      out.push([m.label, m.time]);
    }
  }
  return out;
}

const boardName = (b) => (b.city ? `${b.name}, ${b.city}` : b.name);

/** "Is this the masjid's board?" prompt. */
function suggestionHtml(data) {
  return data.suggestion
    ? `<div class="salah-suggest small">Is <b>${escapeHtml(boardName(data.suggestion))}</b> this masjid's board on masjidboardlive?
        <span class="actions"><button class="btn small" data-board="${escapeHtml(data.suggestion.id)}">Use it</button>
        <button class="btn small ghost" data-board="none">No</button></span></div>`
    : "";
}

/** The card's body for times that loaded. */
function timesHtml(data) {
  const list = todays(data);
  const next = nextEvent(list);
  const now = Date.now();
  const friday = new Date().getDay() === 5;
  const cells = list.map((s, i) => {
    const last = s.jamaat || s.athaan;
    const past = !next?.tomorrow && last && at(last).getTime() < now;
    const cls = next && next.index === i && !next.tomorrow ? "next" : past ? "past" : "";
    const iqamah = s.jamaat && s.jamaat !== s.athaan ? `Iqāmah ${s.jamaat}` : "&nbsp;";
    return `<div class="salah-cell ${cls}">
      <div class="name">${escapeHtml(s.label)}</div>
      <div class="athaan">${escapeHtml(s.athaan || s.jamaat || "—")}</div>
      <div class="jamaat">${iqamah}</div>
    </div>`;
  }).join("");

  let line = "";
  if (next) {
    const s = list[next.index];
    const what = next.kind === "jamaat" ? `${s.label} iqāmah` : `${s.label} adhān`;
    line = next.tomorrow
      ? `Next: <b>${escapeHtml(s.label)}</b> tomorrow at ${escapeHtml(s.athaan || s.jamaat)}`
      : `Next: <b>${escapeHtml(what)}</b> ${inWords(next.when.getTime() - now)} <span class="text-3">(${next.when.toTimeString().slice(0, 5)})</span>`;
  }

  const today = new Date().toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long" });
  const h = hijri(data);
  const head = `<div class="salah-head"><span>${escapeHtml(today)}</span>${h ? `<span class="text-2">${escapeHtml(h)}</span>` : ""}</div>`;

  const extra = dayTimes(data);
  const more = extra.length
    ? `<div class="salah-more">${extra.map(([l, t]) => `<div><span>${escapeHtml(l)}</span><b>${escapeHtml(t)}</b></div>`).join("")}</div>`
    : "";

  const jt = data.jumuah_times || [];
  const jumuah = jt.length
    ? `<div class="salah-line${friday ? " today" : ""}"><span class="key">Jumu'ah</span>
        <span>${jt.map((m) => `${escapeHtml(m.label)} <b>${escapeHtml(m.time)}</b>`).join(" · ")}${data.khateeb ? ` · <span class="text-3">${escapeHtml(data.khateeb)}</span>` : ""}</span></div>`
    : "";
  const changes = (data.changes || []).length
    ? `<div class="salah-line"><span class="key">Changes</span><span>${data.changes.map((c) =>
        `${escapeHtml(c.label)} <b>${escapeHtml(c.time)}</b> from ${escapeHtml(c.date)}`).join(" · ")}</span></div>`
    : "";

  return `${suggestionHtml(data)}${head}<div class="salah-grid">${cells}</div><div class="salah-next">${line}</div>${more}${jumuah}${changes}`;
}

/**
 * One salah card. `root` holds a `.salah-title` (with a `[data-salah-source]` link and a
 * `[data-salah-change]` button) and a `.salah` box.
 */
export class SalahCard {
  constructor(root) {
    this.title = root.querySelector(".salah-title");
    this.box = root.querySelector(".salah");
    this.source = root.querySelector("[data-salah-source]");
    this.change = root.querySelector("[data-salah-change]");
    this.data = null;
    this.mount = null;
    this.retryTimer = null;
    this.box.addEventListener("click", (e) => {
      const b = e.target.closest("[data-board]");
      if (b) this.choose(b.dataset.board);
      if (e.target.closest("[data-salah-retry]")) this.load(this.mount);
    });
    this.change.addEventListener("click", () => openChooser(this));
    cards.push(this);
  }

  /** Load (or reload) times for `mount`. The Pi caches, so this is cheap to call. */
  async load(mount) {
    if (!mount) return this.hide();
    this.mount = mount;
    let data;
    try {
      data = await api(`salah?mount=${encodeURIComponent(mount)}`);
    } catch {
      data = { status: "error", message: "Couldn't load salah times" };
    }
    if (this.mount !== mount) return; // another masjid was chosen meanwhile
    this.data = data;
    this.render();
  }

  /** Re-render (countdown, Hijri date at Maghrib) without fetching. */
  tick() {
    if (this.data?.status === "ok" && !this.box.hidden) this.render();
  }

  hide() {
    this.mount = null;
    this.title.hidden = true;
    this.box.hidden = true;
  }

  render() {
    const data = this.data;
    this.title.hidden = false;
    this.box.hidden = false;
    this.source.textContent = data.provider === "masjidboardlive" ? "masjidboardlive" : "masjidscreen.co.za";
    this.source.href = data.source || "https://masjidscreen.co.za/";
    this.change.hidden = !this.mount;
    clearTimeout(this.retryTimer);
    if (data.status === "ok") {
      this.box.innerHTML = timesHtml(data);
      return;
    }
    // Fallback: the card stays, with a calm one-line explanation (and a retry for errors).
    const retry = data.status === "error" || data.status === "blocked"
      ? ' <button class="btn ghost small" data-salah-retry>Retry</button>' : "";
    this.box.innerHTML = suggestionHtml(data)
      + emptyState(data.message || "Salah times unavailable", { small: true }).replace(/<\/div>$/, `${retry}</div>`);
    if (data.status === "error") this.retryTimer = setTimeout(() => this.load(this.mount), 10 * 60 * 1000);
  }

  async choose(board) {
    if (!this.mount) return;
    try {
      this.data = await api("salah/board", { mount: this.mount, board });
      this.render();
      closeModal("salah-modal");
      toast(board === "none" ? "Using masjidscreen.co.za" : board === "auto" ? "Matching automatically" : `Using ${boardName(this.data.board || { name: "the board" })}`);
      // Other cards showing the same masjid follow the new choice.
      for (const c of cards) if (c !== this && c.mount === this.mount) c.load(c.mount);
    } catch (err) {
      toast(err.message);
    }
  }
}

function openChooser(card) {
  editing = card;
  const data = card.data;
  $("#salah-board-input").value = data?.board && data.manual ? data.board.url : "";
  $("#salah-current").textContent = data?.provider === "masjidboardlive" && data.board
    ? `Now: masjidboardlive · ${boardName(data.board)}${data.manual ? "" : " (matched automatically)"}`
    : `Now: masjidscreen.co.za${data?.manual ? " (chosen)" : ""}`;
  openModal("salah-modal", "#salah-board-input");
}

/** Wire the shared "Change source" dialog and the countdown. Once, at start-up. */
export function init() {
  $("#salah-form").addEventListener("submit", (e) => {
    e.preventDefault();
    editing?.choose($("#salah-board-input").value.trim() || "auto");
  });
  $("#salah-auto").addEventListener("click", () => editing?.choose("auto"));
  $("#salah-none").addEventListener("click", () => editing?.choose("none"));
  // Countdown to the next salah, and a periodic refresh (the Pi serves it from cache).
  setInterval(() => cards.forEach((c) => c.tick()), 30 * 1000);
  setInterval(() => cards.forEach((c) => c.mount && c.load(c.mount)), 30 * 60 * 1000);
}
