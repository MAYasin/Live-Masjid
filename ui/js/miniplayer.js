// The speaker's "other" player: a Quran surah/playlist or a past recording. Rendered as a
// compact card under the masjid card (home) and at the top of the Quran page, from the live
// status. Built once per container, then only updated (so dragging the seek bar isn't
// disturbed by status pushes).

import { api } from "./api.js";
import { runAction, toast } from "./dom.js";

const icon = (id) => `<svg aria-hidden="true"><use href="#i-${id}"/></svg>`;

export function clock(s) {
  s = Math.max(0, Math.floor(s || 0));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}

/** Build the player inside `box` and wire its controls. */
export function mount(box) {
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
    </div>
    <div class="mp-resume" data-f="resume" hidden>
      <button class="btn small" data-act="continue">${icon("play")}Continue now</button>
      <button class="btn small ghost" data-act="dismiss">Don't continue</button>
    </div>`;
  const f = (name) => box.querySelector(`[data-f="${name}"]`);
  box._f = f;
  box._seeking = false;
  const seek = f("seek");
  seek.addEventListener("input", () => {
    box._seeking = true;
    f("pos").textContent = clock(Number(seek.value));
    seek.style.setProperty("--fill", `${(100 * Number(seek.value)) / Math.max(1, Number(seek.max))}%`);
  });
  seek.addEventListener("change", () => {
    runAction(null, () => api("quran/seek", { position_s: Number(seek.value) }))
      .finally(() => setTimeout(() => { box._seeking = false; }, 1500));
  });
  box.addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]");
    if (!b) return;
    const s = box._status;
    const r = s?.recording;
    const q = s?.quran_resume;
    switch (b.dataset.act) {
      case "toggle": if (r) runAction(b, () => api("recordings/pause", { pause: !r.paused })); break;
      case "prev": runAction(b, () => api("quran/step", { by: -1 })); break;
      case "next": runAction(b, () => api("quran/step", { by: 1 })); break;
      case "stop": runAction(b, () => api("recordings/stop", {})); break;
      case "dismiss": runAction(b, () => api("recordings/stop", {}), "The Quran won't continue"); break;
      case "continue":
        if (!q) break;
        runAction(b, () => api("quran/play", q.playlist
          ? { playlist: q.playlist.id, index: q.playlist.index, position_s: q.position_s }
          : { surah: q.surah, position_s: q.position_s })).catch((err) => toast(err.message));
        break;
    }
  });
}

/** Update `box` from the status; hides it when nothing (or a non-Quran thing, with `quranOnly`) plays. */
export function update(box, s, { quranOnly = false } = {}) {
  if (!box._f) mount(box);
  box._status = s;
  const f = box._f;
  const r = s?.recording && (!quranOnly || s.recording.kind === "quran") ? s.recording : null;
  const q = s?.quran_resume;
  box.hidden = !r && !q;
  if (box.hidden) return;

  if (r) {
    const quran = r.kind === "quran";
    const pl = r.playlist;
    f("source").textContent = quran
      ? (pl ? `${pl.name} · ${pl.index + 1} of ${pl.surahs.length}${pl.repeat ? " · repeat" : ""}` : "Quran · Abdul Rahman Al-Sudais")
      : "Recording";
    f("title").textContent = r.title;
    const loading = r.loaded_pct != null ? `Downloading to the Pi: ${r.loaded_pct}%` : "";
    f("note").hidden = !loading && !r.paused;
    f("note").textContent = loading || (r.paused ? "Paused" : "");
    const dur = r.duration_s || 0;
    const seek = f("seek");
    seek.max = String(Math.max(1, dur));
    seek.disabled = !quran || !dur;
    if (!box._seeking) seek.value = String(Math.min(r.position_s, dur || r.position_s));
    seek.style.setProperty("--fill", `${dur ? Math.min(100, (100 * Number(seek.value)) / dur) : 0}%`);
    if (!box._seeking) f("pos").textContent = clock(r.position_s);
    f("dur").textContent = dur ? clock(dur) : "–:––";
    const toggle = box.querySelector('[data-act="toggle"]');
    toggle.innerHTML = icon(r.paused ? "play" : "pause");
    toggle.setAttribute("aria-label", r.paused ? "Play" : "Pause");
    toggle.title = r.paused ? "Play" : "Pause";
    for (const act of ["prev", "next"]) box.querySelector(`[data-act="${act}"]`).hidden = !quran;
    f("seekrow").hidden = false;
    f("controls").hidden = false;
    f("resume").hidden = true;
  } else {
    const pl = q.playlist;
    f("source").textContent = pl ? `${pl.name} · ${pl.index + 1} of ${pl.surahs.length}` : "Quran · Abdul Rahman Al-Sudais";
    f("title").textContent = q.title;
    f("note").hidden = false;
    f("note").textContent = q.auto
      ? `Paused at ${clock(q.position_s)} for the live broadcast. It continues by itself when the broadcast ends.`
      : `Paused at ${clock(q.position_s)}.`;
    f("seekrow").hidden = true;
    f("controls").hidden = true;
    f("resume").hidden = false;
  }
}
