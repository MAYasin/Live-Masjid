// Small building blocks shared by the views: markup for common pieces, and a poller that only
// runs while its page is open.

import { $ } from "./dom.js";
import { escapeHtml } from "./format.js";

/** An inline icon from the sprite in index.html. */
export const icon = (name) => `<svg><use href="#i-${name}"/></svg>`;

/** The "nothing here" message. `small` for messages inside cards. */
export const emptyState = (text, { small = false } = {}) =>
  `<div class="empty${small ? " small" : ""}">${escapeHtml(text)}</div>`;

/** Fill a <dl> (by id) with [label, value] rows, skipping empty values. */
export function definitionList(id, rows) {
  $(`#${id}`).innerHTML = rows
    .filter(([, v]) => v !== null && v !== undefined && v !== "")
    .map(([k, v]) => `<dt>${escapeHtml(k)}</dt><dd>${escapeHtml(v)}</dd>`)
    .join("");
}

/** Set a meter (`#<id>-bar`) to `percent`, amber from `warnAt`, red from `critAt`. */
export function meter(id, percent, warnAt = 75, critAt = 90) {
  const bar = $(`#${id}-bar`);
  const p = Math.max(0, Math.min(100, percent ?? 0));
  bar.style.width = `${p}%`;
  bar.className = p >= critAt ? "critical" : p >= warnAt ? "high" : "";
}

/** Is the page `#view-<view>` the one showing? */
export const isOpen = (view) => Boolean(document.querySelector(`#view-${view}.active`));

const pollers = new Map();

/**
 * Call `fn` every `ms` while the page `#view-<view>` is open (skipped while the browser tab is
 * hidden). Calling it again for the same page replaces the previous timer; the timer stops by
 * itself once another page is opened.
 */
export function pollWhileOpen(view, fn, ms) {
  clearInterval(pollers.get(view));
  const timer = setInterval(() => {
    if (!isOpen(view)) {
      clearInterval(timer);
      pollers.delete(view);
      return;
    }
    if (!document.hidden) fn();
  }, ms);
  pollers.set(view, timer);
}
