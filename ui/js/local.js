// Listening on *this* device (phone/laptop), independent of the Pi's speaker. Two modes:
//
//  - "sync" (default): plays the Pi's own output (/api/listen/live.wav), paced by the Pi's
//    audio device, and holds the browser's buffer so it sounds at the same moment as the speaker.
//  - "direct": livemasjid's MP3 stream, fetched by us so we can drop Icecast's connect burst
//    (~17 s of old audio) and play ~1.5 s behind live (MediaSource). Browsers without
//    MediaSource play the stream as-is (further behind).

import { api } from "./api.js";
import { $, toast } from "./dom.js";
import { escapeHtml } from "./format.js";

const SYNC_URL = "/api/listen/live.wav";
// The speaker plays ~0.85 s after the Pi hands audio over (writer queue, pipe, aplay 250 ms,
// Bluetooth). Holding the same amount buffered here puts both at the same moment; the user
// can fine-tune it (device output latency varies, e.g. Bluetooth headphones).
const SYNC_TARGET_S = 0.85;
// Direct mode: how much audio to keep from the connect burst / hold buffered.
const DIRECT_TARGET_S = 1.5;
const MIN_TARGET_S = 0.3;

/** @type {null | { kind: "live" | "recording", title: string, mount?: string, mode: "sync" | "direct" | "plain" }} */
let current = null;
const listeners = new Set();
let timer;
let recent = []; // last ~2 s of buffered-ahead samples
let lastJump = 0;
let direct = null; // { ctrl, ms, sb, queue, objUrl }

const audio = () => $("#local-audio");

function load(key, fallback) {
  try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
}
function save(key, value) {
  try { localStorage.setItem(key, value); } catch { /* private mode */ }
}

let mode = load("lm.listenMode", "sync") === "direct" ? "direct" : "sync";
let offset = Number(load("lm.syncOffset", "0")) || 0;

/** Call `fn(current)` whenever local playback starts/stops. */
export function onChange(fn) {
  listeners.add(fn);
}

export const isPlaying = () => current !== null;

function changed() {
  for (const fn of listeners) fn(current);
  $("#local-bar").hidden = !current;
  if (!current) return;
  const how = { sync: "in sync with the speaker", direct: "direct from livemasjid, not synced", plain: "direct from livemasjid" }[current.mode];
  const what = current.kind === "recording" ? "Playing on this device" : `Listening on this device (${how})`;
  $("#local-title").innerHTML = `${what} · <b>${escapeHtml(current.title)}</b>`;
  $("#local-controls").hidden = current.kind !== "live";
  for (const b of document.querySelectorAll("#local-mode [data-mode]")) b.classList.toggle("active", b.dataset.mode === mode);
  $("#sync-adjust").hidden = current.mode !== "sync";
  $("#sync-offset").textContent = `${offset > 0 ? "+" : ""}${offset.toFixed(1)} s`;
}

// ------------------------------------------------------------------ keeping the buffer on target

function keepUp() {
  const el = audio();
  if (!current || current.mode === "plain" || current.kind !== "live" || el.paused || !el.buffered.length) return;
  const end = el.buffered.end(el.buffered.length - 1);
  const ahead = end - el.currentTime;
  const target = current.mode === "sync" ? Math.max(MIN_TARGET_S, SYNC_TARGET_S + offset) : DIRECT_TARGET_S;
  // Browsers move downloaded audio into `buffered` in ~0.5 s steps, so `ahead` is a sawtooth
  // whose *peak* is the real distance to the newest audio. Steer on the recent peak.
  recent.push(ahead);
  if (recent.length > 8) recent.shift();
  const off = Math.max(...recent) - target;
  // Well behind (the browser prebuffers ~2.5 s before it starts, or a network stall): jump
  // within what's already downloaded, then fine-tune with the playback rate. Chrome tops
  // its buffer back up from what was waiting in the network.
  if (off > 0.8 && performance.now() - lastJump > 2000) {
    lastJump = performance.now();
    el.currentTime = end - target;
    recent = [];
    return;
  }
  if (current.mode === "direct") trimDirect(el);
  // Proportional: a few % faster/slower with pitch preserved is inaudible on speech.
  let rate = Math.abs(off) < 0.04 ? 1 : 1 + Math.max(-0.03, Math.min(0.08, off * 0.12));
  rate = Math.round(rate * 200) / 200;
  if (el.playbackRate !== rate) el.playbackRate = rate;
}

// ------------------------------------------------------------------ direct mode (MediaSource)

/** One MPEG audio Layer III frame header → [length in bytes, seconds], or null. */
function mp3Frame(b, i) {
  if (i + 4 > b.length || b[i] !== 0xff || (b[i + 1] & 0xe0) !== 0xe0) return null;
  const version = (b[i + 1] >> 3) & 3, layer = (b[i + 1] >> 1) & 3;
  const br = b[i + 2] >> 4, sr = (b[i + 2] >> 2) & 3, pad = (b[i + 2] >> 1) & 1;
  if (version === 1 || layer !== 1 || br === 0 || br === 15 || sr === 3) return null;
  const v1 = version === 3;
  const kbps = (v1 ? [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320] : [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160])[br];
  const rate = [44100, 48000, 32000][sr] / (v1 ? 1 : version === 2 ? 2 : 4);
  const samples = v1 ? 1152 : 576;
  return [Math.floor(((v1 ? 144 : 72) * kbps * 1000) / rate) + pad, samples / rate];
}

/** Offset in `b` leaving about `keepS` seconds of whole MP3 frames (0 if unsure). */
function mp3Start(b, keepS) {
  let first = -1;
  for (let i = 0; i + 4 <= b.length; i++) {
    const f = mp3Frame(b, i);
    if (f && mp3Frame(b, i + f[0])) { first = i; break; }
  }
  if (first < 0) return 0;
  const frames = [];
  for (let i = first, f; (f = mp3Frame(b, i)) && i + f[0] <= b.length; i += f[0]) frames.push([i, f[1]]);
  let have = 0;
  for (let k = frames.length - 1; k >= 0; k--) {
    have += frames[k][1];
    if (have >= keepS) return frames[k][0];
  }
  return first;
}

function concat(parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
}

function feed(d) {
  if (!d.sb || d.sb.updating || !d.queue.length || d.ms.readyState !== "open") return;
  const data = concat(d.queue);
  d.queue = [];
  try { d.sb.appendBuffer(data); } catch { /* quota: trimDirect frees space, data is dropped */ }
}

function trimDirect(el) {
  const sb = direct?.sb;
  if (!sb || sb.updating || !sb.buffered.length) return;
  const start = sb.buffered.start(0);
  if (el.currentTime - start > 30) sb.remove(start, el.currentTime - 10);
}

/** Read the stream: drop the connect burst except the newest DIRECT_TARGET_S, then append. */
async function readDirect(d, reader) {
  let burst = [];
  const t0 = performance.now();
  for (;;) {
    const t = performance.now();
    let r;
    try { r = await reader.read(); } catch { break; }
    if (r.done) break;
    if (burst) {
      burst.push(r.value);
      const waited = performance.now() - t > 400; // burst over: now waiting for new audio
      if ((burst.length > 1 && waited) || performance.now() - t0 > 3000 || burst.length > 400) {
        const all = concat(burst);
        burst = null;
        d.queue.push(all.subarray(mp3Start(all, DIRECT_TARGET_S)));
        feed(d);
      }
      continue;
    }
    d.queue.push(r.value);
    feed(d);
  }
  if (direct === d && !d.ctrl.signal.aborted) {
    toast("The stream ended");
    stop();
  }
}

async function startDirect(urls) {
  const el = audio();
  const ms = new MediaSource();
  const d = { ctrl: new AbortController(), ms, sb: null, queue: [], objUrl: URL.createObjectURL(ms) };
  direct = d;
  el.innerHTML = "";
  el.src = d.objUrl;
  const playing = el.play(); // inside the tap's user activation
  playing.catch(() => {}); // handled by begin() when returned; avoid an unhandled rejection otherwise
  await new Promise((res) => ms.addEventListener("sourceopen", res, { once: true }));
  if (direct !== d) return playing;
  d.sb = ms.addSourceBuffer("audio/mpeg");
  d.sb.mode = "sequence";
  d.sb.addEventListener("updateend", () => feed(d));
  let res = null;
  for (const url of urls) {
    try {
      const r = await fetch(url, { signal: d.ctrl.signal, cache: "no-store" });
      if (r.ok && r.body) { res = r; break; }
    } catch {
      if (d.ctrl.signal.aborted) return playing;
    }
  }
  if (!res) throw new Error("stream unavailable");
  readDirect(d, res.body.getReader());
  return playing;
}

function endDirect() {
  if (!direct) return;
  direct.ctrl.abort();
  URL.revokeObjectURL(direct.objUrl);
  direct = null;
}

const canDirect = () => typeof window.MediaSource === "function" && MediaSource.isTypeSupported("audio/mpeg");

// ------------------------------------------------------------------ start / stop

function reset() {
  const el = audio();
  clearInterval(timer);
  endDirect();
  recent = [];
  el.pause();
  el.playbackRate = 1;
  el.removeAttribute("src");
  el.innerHTML = "";
}

async function begin(next, setup) {
  reset();
  current = next;
  const el = audio();
  el.preservesPitch = true;
  timer = setInterval(keepUp, 250);
  changed();
  try {
    await setup(el);
  } catch (err) {
    if (current !== next) return;
    reset();
    current = null;
    changed();
    toast(err?.name === "NotAllowedError" ? "Tap again to allow audio" : "Couldn't play on this device");
  }
}

/** Play the live stream of `mount` (or the selected masjid) on this device. */
export async function playLive(mount, title) {
  const q = mount ? `?mount=${encodeURIComponent(mount)}` : "";
  const info = await api(`listen${q}`);
  if (!info.live) {
    toast("The masjid isn't live right now");
    return;
  }
  const others = info.sources.filter((s) => !s.synced);
  const sourceTags = (list) => list.map((s) => `<source src="${escapeHtml(s.url)}" type="${escapeHtml(s.type)}">`).join("");
  const name = title || info.mount;
  if (mode === "direct" && canDirect()) {
    const mp3 = others.filter((s) => s.type === "audio/mpeg").map((s) => s.url);
    return begin({ kind: "live", title: name, mount: info.mount, mode: "direct" }, () => startDirect(mp3));
  }
  // Sync mode lists the Pi's mirror first; the plain streams are the fallback.
  const list = mode === "sync" ? info.sources : others;
  const plain = mode !== "sync" || !info.sources.some((s) => s.synced);
  return begin({ kind: "live", title: name, mount: info.mount, mode: plain ? "plain" : "sync" }, (el) => {
    el.innerHTML = sourceTags(list);
    el.load();
    return el.play().then(() => {
      // Fell through to a livemasjid source: not in sync.
      if (current && current.mode === "sync" && !el.currentSrc.includes(SYNC_URL)) { current.mode = "plain"; changed(); }
    });
  });
}

/** Play a listed recording on this device. */
export async function playRecording(mount, id) {
  const r = await api(`recordings/url?mount=${encodeURIComponent(mount)}&id=${encodeURIComponent(id)}`);
  return begin({ kind: "recording", title: r.title, mode: "plain" }, (el) => {
    el.src = r.url;
    return el.play();
  });
}

export function stop() {
  reset();
  audio().load();
  current = null;
  changed();
}

function setMode(next) {
  if (next === mode) return;
  mode = next;
  save("lm.listenMode", mode);
  if (current?.kind === "live") playLive(current.mount, current.title).catch((e) => toast(e.message));
  else changed();
}

export function init() {
  $("#local-stop").addEventListener("click", stop);
  $("#local-mode").addEventListener("click", (e) => {
    const b = e.target.closest("[data-mode]");
    if (b) setMode(b.dataset.mode);
  });
  $("#sync-adjust").addEventListener("click", (e) => {
    const b = e.target.closest("[data-nudge]");
    if (!b) return;
    offset = Math.max(-0.5, Math.min(1.5, Math.round((offset + Number(b.dataset.nudge)) * 10) / 10));
    save("lm.syncOffset", String(offset));
    changed();
  });
  const el = audio();
  el.addEventListener("ended", () => { if (!direct) stop(); });
  el.addEventListener("error", () => {
    // With <source> children the element only errors once every source failed.
    if (current && !direct) {
      toast("This device couldn't play the stream");
      stop();
    }
  });
}
