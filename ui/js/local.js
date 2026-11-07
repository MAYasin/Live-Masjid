// Listening on *this* device (phone/laptop), independent of the Pi's speaker.
// Uses one <audio> element; live streams come with several sources (MP3 first) and the
// browser falls through to the next one if a source fails.

import { api } from "./api.js";
import { $, toast } from "./dom.js";
import { escapeHtml } from "./format.js";

let current = null; // { kind: "live" | "recording", title }
const listeners = new Set();

const audio = () => $("#local-audio");

/** Call `fn(current)` whenever local playback starts/stops. */
export function onChange(fn) {
  listeners.add(fn);
}

function changed() {
  for (const fn of listeners) fn(current);
  const bar = $("#local-bar");
  bar.hidden = !current;
  if (current) $("#local-title").innerHTML = `Listening on this device · <b>${escapeHtml(current.title)}</b>`;
}

export const isPlaying = () => current !== null;

function start(title, kind) {
  current = { kind, title };
  const el = audio();
  el.load();
  el.play().then(changed).catch((err) => {
    current = null;
    changed();
    toast(err?.name === "NotAllowedError" ? "Tap again to allow audio" : "Couldn't play on this device");
  });
  changed();
}

/** Play the live stream of `mount` (or the selected masjid) on this device. */
export async function playLive(mount, title) {
  const q = mount ? `?mount=${encodeURIComponent(mount)}` : "";
  const info = await api(`listen${q}`);
  if (!info.live) {
    toast("The masjid isn't live right now");
    return;
  }
  const el = audio();
  el.innerHTML = info.sources.map((s) => `<source src="${escapeHtml(s.url)}" type="${escapeHtml(s.type)}">`).join("");
  el.removeAttribute("src");
  start(title || info.mount, "live");
}

/** Play a listed recording on this device. */
export async function playRecording(mount, id) {
  const r = await api(`recordings/url?mount=${encodeURIComponent(mount)}&id=${encodeURIComponent(id)}`);
  const el = audio();
  el.innerHTML = "";
  el.src = r.url;
  start(r.title, "recording");
}

export function stop() {
  const el = audio();
  el.pause();
  el.removeAttribute("src");
  el.innerHTML = "";
  el.load();
  current = null;
  changed();
}

export function init() {
  $("#local-stop").addEventListener("click", stop);
  audio().addEventListener("ended", stop);
  audio().addEventListener("error", () => {
    // With <source> children the element only errors once every source failed.
    if (current) {
      toast("This device couldn't play the stream");
      stop();
    }
  });
}
