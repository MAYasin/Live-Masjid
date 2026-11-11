// Formatting helpers (pure functions, no DOM).

const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

/** Escape a value for safe interpolation into HTML. */
export const escapeHtml = (value) => String(value ?? "").replace(/[&<>"']/g, (c) => ESCAPES[c]);

/** "14:05" style local time. */
export const clockTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

/** "12 Nov" style date. */
export const shortDate = (ms) => new Date(ms).toLocaleDateString([], { day: "numeric", month: "short" });

/** Relative time, e.g. "Just now", "5 min ago", "Yesterday, 18:02". */
export function timeAgo(ms) {
  if (!ms) return "Never";
  const s = (Date.now() - ms) / 1000;
  if (s < 45) return "Just now";
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))} min ago`;
  if (s < 86400) {
    const h = Math.round(s / 3600);
    return `${h} ${h === 1 ? "hour" : "hours"} ago`;
  }
  if (s < 172800) return `Yesterday, ${clockTime(ms)}`;
  if (s < 604800) return `${new Date(ms).toLocaleDateString([], { weekday: "long" })}, ${clockTime(ms)}`;
  return `${shortDate(ms)}, ${clockTime(ms)}`;
}

/** Lower-case first letter, for use mid-sentence: "Last heard just now". */
export const lowerFirst = (s) => s.charAt(0).toLowerCase() + s.slice(1);

/** Duration, e.g. "<1 min", "42 min", "1h 05m". */
export function duration(ms) {
  const m = Math.max(0, Math.round(ms / 60000));
  if (m < 1) return "<1 min";
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

/** "Saturday, 7 November" style day heading. */
export const dayHeading = (ms) =>
  new Date(ms).toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" });

/** Playback position, e.g. "3:07" or "1:02:45". */
export function clock(s) {
  s = Math.max(0, Math.floor(s || 0));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}

/** How long something has been up, e.g. "3 d 4 h", "5 h 12 m" or "8 m". */
export function uptime(s) {
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  if (d) return `${d} d ${h} h`;
  if (h) return `${h} h ${m} m`;
  return `${m} m`;
}

/** A file size in bytes, e.g. "4.2 MB" (one decimal below 10 MB). */
export const fileSize = (b) => `${(b / 1048576).toFixed(b < 10485760 ? 1 : 0)} MB`;

/** A size in megabytes, switching to GB from 1024 MB. */
export const storageSize = (mb) => (mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`);
