// One masjid's page (#masjid/<mount>, opened from the Masjids list or the home card): follow or
// listen, today's salah times, and its recordings. "#masjid/<mount>/recordings" scrolls to them.

import { api } from "../api.js";
import { emptyState } from "../components.js";
import { $, toast } from "../dom.js";
import { lowerFirst, timeAgo } from "../format.js";
import { subscribe } from "../store.js";
import { shortPlace } from "./masjids.js";
import * as recordings from "./recordings.js";
import { SalahCard } from "./salah.js";

let mount = null;
let station = null;
let salahCard;
let navigate = () => {};

export function init(go) {
  navigate = go;
  salahCard = new SalahCard($("#md-salah"));
  $("#rec-list").addEventListener("click", recordings.onPlayClick);
  $("#rec-refresh").addEventListener("click", loadRecordings);
  $("#md-follow").addEventListener("click", () => choose(false));
  $("#md-listen").addEventListener("click", () => choose(true));
  // "Following" and live state change while the page is open.
  subscribe((s) => {
    if (!station) return;
    station.selected = s.mount === station.mount;
    paintHeader();
  });
}

export async function show(arg = "") {
  const [m, section] = arg.split("/");
  mount = m || null;
  station = null;
  if (!mount) {
    navigate("masjids");
    return;
  }
  $("#md-name").textContent = mount;
  $("#md-crumb").textContent = mount;
  $("#md-place").textContent = "";
  $("#md-status").textContent = "";
  for (const b of ["#md-listen", "#md-follow"]) $(b).hidden = true;
  $("#md-following").hidden = true;
  salahCard.load(mount);
  const [stations] = await Promise.all([api("stations").catch(() => null), loadRecordings()]);
  station = stations?.stations.find((s) => s.mount === mount) ?? { mount, name: mount, location: "", live: false, selected: false };
  paintHeader();
  if (section === "recordings") $("#md-recordings-title").scrollIntoView({ block: "start" });
}

function paintHeader() {
  const s = station;
  $("#md-name").textContent = s.name;
  $("#md-crumb").textContent = s.name.split(",")[0].trim();
  $("#md-place").textContent = shortPlace(s.location) || s.genre || "";
  const heard = s.last_heard_ms ? `Last heard ${lowerFirst(timeAgo(s.last_heard_ms))}` : "";
  $("#md-status").innerHTML = s.live
    ? '<span class="dot ok"></span><span>Live now</span>'
    : `<span class="dot"></span><span>${heard || "Not live"}</span>`;
  $("#md-follow").hidden = s.selected;
  $("#md-listen").hidden = !s.live;
  $("#md-listen").lastChild.textContent = s.selected ? "Listen now" : "Follow and listen now";
  $("#md-following").hidden = !s.selected;
}

async function loadRecordings() {
  const box = $("#rec-list");
  const forMount = mount;
  box.innerHTML = '<div class="empty"><span class="spinner"></span></div>';
  try {
    const list = await api(`recordings?mount=${encodeURIComponent(forMount)}`);
    if (forMount !== mount) return;
    box.innerHTML = list.status === "ok"
      ? recordings.rows(list) || emptyState("No recordings yet.", { small: true })
      : recordings.fallback(list);
  } catch (err) {
    if (forMount === mount) box.innerHTML = recordings.fallback({ message: err.message });
  }
}

/** Follow this masjid (and, with `listen`, play it now), then go to the home screen. */
async function choose(listen) {
  const s = station;
  if (!s) return;
  try {
    if (!s.selected) await api("select", { mount: s.mount });
    if (listen) await api("player/play", {});
    toast(listen ? `Playing ${s.name}` : `Following ${s.name}. It plays when they go live.`);
    navigate("now");
  } catch (err) {
    toast(err.message);
  }
}
