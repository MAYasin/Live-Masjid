// Live player status: one snapshot from the server, kept current over server-sent events.

import { api } from "./api.js";

let status = null;
let source = null;
const listeners = new Set();
const connectionListeners = new Set();

/** The latest status snapshot (or null before the first load). */
export const getStatus = () => status;

/** Call `fn(status)` on every update. Returns an unsubscribe function. */
export function subscribe(fn) {
  listeners.add(fn);
  if (status) fn(status);
  return () => listeners.delete(fn);
}

/** Call `fn(connected)` when the connection to the player changes. */
export function onConnection(fn) {
  connectionListeners.add(fn);
}

function publish(next) {
  status = next;
  for (const fn of listeners) fn(status);
}

function setConnected(connected) {
  for (const fn of connectionListeners) fn(connected);
}

/** Load the first snapshot, then stream updates. Throws if the API is unreachable. */
export async function start() {
  publish(await api("state"));
  setConnected(true);
  connect();
}

function connect() {
  source?.close();
  source = new EventSource("/api/events");
  source.addEventListener("status", (e) => {
    setConnected(true);
    publish(JSON.parse(e.data));
  });
  source.onerror = () => {
    setConnected(false);
    source.close();
    setTimeout(async () => {
      try { await api("state"); } catch { /* still offline; EventSource retries below */ }
      connect();
    }, 2500);
  };
}

// Keep relative times ("2 min ago") fresh even when nothing changes.
setInterval(() => status && publish(status), 15000);
