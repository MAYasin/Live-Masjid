// The speaker's "other" player: a Quran surah/playlist or a past recording. Rendered as a
// compact card under the masjid card (home) and at the top of the Quran page, from the live
// status. Built once per container, then only updated (so dragging the seek bar isn't
// disturbed by status pushes).
//
// Controls are optimistic: a tap changes the card at once (play/pause icon, position, "next…",
// hidden on stop) and the command goes to the Pi in the background. The live status confirms it
// (or, after a few seconds or an error, wins). Between status pushes the clock ticks locally.

import { api } from "./api.js";
import { runAction, toast } from "./dom.js";

const icon = (id) => `<svg aria-hidden="true"><use href="#i-${id}"/></svg>`;
/** How long an optimistic change may wait for the Pi to confirm it. */
const CONFIRM_MS = 4000;
/** Playback speeds the speed button steps through. */
const SPEEDS = [1, 1.25, 1.5, 1.75, 2, 0.75];
const speedLabel = (v) => `${Number(v.toFixed(2))}×`;

/** Every mounted player card (home and Quran page). */
const boxes = new Set();

/** Show `title` as starting in every player card right away (a Play button was tapped). */
export function starting(title, kind = "quran") {
  for (const box of boxes) {
    box._opt = { starting: { title, kind }, until: Date.now() + CONFIRM_MS + 4000 };
    render(box);
  }
}

/** Undo `starting` (the request failed). */
export function cancelStarting() {
  for (const box of boxes) {
    if (box._opt?.starting) {
      box._opt = null;
      render(box);
    }
  }
}

export function clock(s) {
  s = Math.max(0, Math.floor(s || 0));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}

/** Build the player inside `box` and wire its controls. */
export function mount(box) {
  boxes.add(box);
  box.classList.add("mp");
  box.innerHTML = `
    <div class="mp-text">
      <div class="mp-source" data-f="source"></div>
      <div class="mp-title truncate" data-f="title"></div>
      <div class="mp-note" data-f="note" hidden></div>
    </div>
    <div class="mp-seek" data-f="seekrow">
      <span class="tabular" data-f="pos">0:00</span>
      <input type="range" min="0" max="1" value="0" step="1" aria-label="Position" data-f="seek">
      <span class="tabular" data-f="dur">–:––</span>
    </div>
    <div class="mp-controls" data-f="controls">
      <button class="icon-btn" data-act="prev" aria-label="Previous surah" title="Previous">${icon("prev")}</button>
      <button class="icon-btn play" data-act="toggle" aria-label="Pause" title="Pause">${icon("pause")}</button>
      <button class="icon-btn" data-act="next" aria-label="Next surah" title="Next">${icon("next")}</button>
      <button class="icon-btn" data-act="stop" aria-label="Stop" title="Stop">${icon("stop")}</button>
      <button class="speed-btn tabular" data-act="speed" title="Playback speed" aria-label="Playback speed">1×</button>
    </div>
    <div class="mp-volume" data-f="volume" hidden>
      <svg aria-hidden="true"><use href="#i-volume"/></svg>
      <input type="range" min="0" max="100" aria-label="Quran volume" title="Quran volume (the broadcast has its own)" data-f="vol">
      <output class="tabular" data-f="volval">–</output>
    </div>
    <div class="mp-resume" data-f="resume" hidden>
      <button class="btn small" data-act="continue">${icon("play")}Continue now</button>
      <button class="btn small ghost" data-act="dismiss">Don't continue</button>
    </div>`;
  const f = (name) => box.querySelector(`[data-f="${name}"]`);
  box._f = f;
  box._seeking = false;
  box._opt = null; // optimistic change waiting for confirmation
  box._clock = null; // { pos, at, running, dur } for local ticking

  const vol = f("vol");
  let volTimer;
  vol.addEventListener("input", () => {
    box._volBusy = true;
    paintVolume(f, Number(vol.value));
    clearTimeout(volTimer);
    volTimer = setTimeout(() => {
      runAction(null, () => api("settings", { quran_volume: Number(vol.value) })).finally(() => { box._volBusy = false; });
    }, 120);
  });

  const seek = f("seek");
  seek.addEventListener("input", () => {
    box._seeking = true;
    paintPosition(box, Number(seek.value));
  });
  seek.addEventListener("change", () => {
    const to = Number(seek.value);
    box._seeking = false;
    optimistic(box, { position: to }, api("quran/seek", { position_s: to }));
  });

  box.addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]");
    if (!b) return;
    const s = box._status;
    const r = effective(box);
    const q = s?.quran_resume;
    switch (b.dataset.act) {
      case "toggle":
        if (r) optimistic(box, { paused: !r.paused }, api("recordings/pause", { pause: !r.paused }));
        break;
      case "prev":
      case "next": {
        if (!r) break;
        const by = b.dataset.act === "next" ? 1 : -1;
        optimistic(box, { stepping: by, from: r.surah, position: 0, paused: false }, api("quran/step", { by }));
        break;
      }
      case "stop":
        optimistic(box, { stopped: true }, api("recordings/stop", {}));
        break;
      case "speed": {
        if (!r) break;
        const cur = r.speed || 1;
        const next = SPEEDS[(SPEEDS.findIndex((v) => Math.abs(v - cur) < 0.01) + 1) % SPEEDS.length] ?? 1;
        optimistic(box, { speed: next }, api("quran/speed", { speed: next }));
        break;
      }
      case "dismiss":
        optimistic(box, { stopped: true }, api("recordings/stop", {}));
        break;
      case "continue":
        if (!q) break;
        runAction(b, () => api("quran/play", q.playlist
          ? { playlist: q.playlist.id, index: q.playlist.index, position_s: q.position_s }
          : { surah: q.surah, position_s: q.position_s }));
        break;
    }
  });

  // Local clock: advance the position between status pushes while playing.
  box._timer = setInterval(() => {
    if (box.hidden || box._seeking || !box._clock?.running) return;
    const c = box._clock;
    paintPosition(box, Math.min(c.dur || Infinity, c.pos + ((Date.now() - c.at) / 1000) * (c.rate || 1)));
  }, 500);
}

/** Apply `patch` to the card now; send `request`; undo (and say why) if it fails. */
function optimistic(box, patch, request) {
  box._opt = { ...patch, until: Date.now() + CONFIRM_MS };
  render(box);
  request.catch((err) => {
    box._opt = null;
    render(box);
    toast(err.message);
  });
}

/** The recording as the card should show it: the live status plus any unconfirmed change. */
function effective(box) {
  const s = box._status;
  const r = s?.recording && (!box._quranOnly || s.recording.kind === "quran") ? { ...s.recording } : null;
  const o = box._opt;
  if (!o) return r;
  if (o.starting) {
    // A Play button was tapped: show it until the Pi reports that recording/surah.
    if ((r && r.title === o.starting.title) || Date.now() > o.until) {
      box._opt = null;
      return r;
    }
    if (box._quranOnly && o.starting.kind !== "quran") return r;
    return { kind: o.starting.kind, title: o.starting.title, position_s: 0, duration_s: 0, paused: false, playlist: null, reciter: null, loaded_pct: null, starting: true };
  }
  // Confirmed (the Pi now reports what we expected) or expired: drop the overlay.
  const confirmed = o.stopped ? !r
    : o.stepping ? r && r.surah !== o.from
    : !r ? false
    : "speed" in o ? Math.abs((r.speed || 1) - o.speed) < 0.01
    : ("paused" in o && !("position" in o)) ? r.paused === o.paused
    : "position" in o ? Math.abs(r.position_s - o.position) < 4
    : true;
  if (confirmed || Date.now() > o.until) {
    box._opt = null;
    return r;
  }
  if (!r || o.stopped) return null;
  if ("paused" in o) r.paused = o.paused;
  if ("speed" in o) r.speed = o.speed;
  if ("position" in o) r.position_s = o.position;
  if (o.stepping) {
    r.title = o.stepping > 0 ? "Next surah…" : "Previous surah…";
    r.loaded_pct = null;
  }
  return r;
}

function paintPosition(box, pos) {
  const f = box._f;
  const seek = f("seek");
  const dur = Number(seek.max) || 1;
  seek.value = String(Math.min(pos, dur));
  seek.style.setProperty("--fill", `${Math.min(100, (100 * pos) / dur)}%`);
  f("pos").textContent = clock(pos);
}

function paintVolume(f, v) {
  f("vol").style.setProperty("--fill", `${v}%`);
  f("volval").textContent = v;
}

/** Update `box` from the status; hides it when nothing (or a non-Quran thing, with `quranOnly`) plays.
 *  `volume` adds a volume slider (pages without their own). */
export function update(box, s, { quranOnly = false, volume = false } = {}) {
  if (!box._f) mount(box);
  box._status = s;
  box._quranOnly = quranOnly;
  box._volume = volume;
  render(box);
}

function render(box) {
  const s = box._status;
  const f = box._f;
  const r = effective(box);
  const q = box._opt?.stopped ? null : s?.quran_resume;
  const pending = s?.quran_pending;
  box.hidden = !r && !q && !pending;
  if (box.hidden) {
    box._clock = null;
    return;
  }
  // The Quran has its own volume (recitations are louder than broadcasts).
  const quranOn = (r && r.kind === "quran") || (!r && q);
  f("volume").hidden = !quranOn;
  if (quranOn && !box._volBusy && s) {
    f("vol").value = String(s.quran_volume ?? 100);
    paintVolume(f, s.quran_volume ?? 100);
  }
  if (!r && !q) {
    // Only a scheduled playlist waiting for the broadcast.
    f("source").textContent = "Scheduled";
    f("title").textContent = pending;
    f("note").hidden = false;
    f("note").textContent = "Starts when the live broadcast ends.";
    f("seekrow").hidden = true;
    f("controls").hidden = true;
    f("resume").hidden = true;
    box._clock = null;
    return;
  }

  if (r) {
    const quran = r.kind === "quran";
    const pl = r.playlist;
    f("source").textContent = quran
      ? [pl ? `${pl.name}, ${pl.index + 1} of ${pl.surahs.length}` : "Quran", r.reciter].filter(Boolean).join(" · ")
      : "Recording";
    f("title").textContent = r.title;
    const loading = r.starting ? "Starting on the speaker…" : r.loaded_pct != null ? `Downloading to the Pi: ${r.loaded_pct}%` : "";
    const waiting = pending ? `"${pending}" starts when the live broadcast ends.` : "";
    f("note").hidden = !loading && !r.paused && !waiting;
    f("note").textContent = loading || waiting || (r.paused ? "Paused" : "");
    const dur = r.duration_s || 0;
    const seek = f("seek");
    seek.max = String(Math.max(1, dur));
    seek.disabled = !quran || !dur;
    f("dur").textContent = dur ? clock(dur) : "–:––";
    box._clock = { pos: r.position_s, at: Date.now(), running: !r.paused && !box._opt?.stepping && !r.starting, dur, rate: r.speed || 1 };
    if (!box._seeking) paintPosition(box, Math.min(r.position_s, dur || r.position_s));
    const toggle = box.querySelector('[data-act="toggle"]');
    toggle.innerHTML = icon(r.paused ? "play" : "pause");
    toggle.setAttribute("aria-label", r.paused ? "Play" : "Pause");
    toggle.title = r.paused ? "Play" : "Pause";
    for (const act of ["prev", "next", "speed"]) box.querySelector(`[data-act="${act}"]`).hidden = !quran;
    const sp = box.querySelector('[data-act="speed"]');
    sp.textContent = speedLabel(r.speed || 1);
    sp.classList.toggle("on", Math.abs((r.speed || 1) - 1) > 0.01);
    f("seekrow").hidden = false;
    f("controls").hidden = false;
    f("resume").hidden = true;
  } else {
    const pl = q.playlist;
    f("source").textContent = pl ? `${pl.name}, ${pl.index + 1} of ${pl.surahs.length}` : "Quran";
    f("title").textContent = q.title;
    f("note").hidden = false;
    f("note").textContent = q.auto
      ? `Paused at ${clock(q.position_s)} for the live broadcast. It continues by itself when the broadcast ends.`
      : `Paused at ${clock(q.position_s)}.`;
    f("seekrow").hidden = true;
    f("controls").hidden = true;
    f("resume").hidden = false;
    box._clock = null;
  }
}
