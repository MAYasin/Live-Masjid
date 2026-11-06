// Small DOM helpers shared by all views.

export const $ = (selector, root = document) => root.querySelector(selector);
export const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

/** Mark exactly one button in a group as active. */
export function setActive(buttons, active) {
  for (const b of buttons) b.classList.toggle("active", b === active);
}

let toastTimer;
/** Show a short message at the bottom of the screen. */
export function toast(message) {
  const el = $("#toast");
  el.textContent = message;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 2600);
}

/**
 * Run an async action, showing a spinner on `button` meanwhile and a toast afterwards.
 * `success` may be a string or a function of the result.
 */
export async function runAction(button, action, success) {
  const html = button?.innerHTML;
  if (button) {
    button.disabled = true;
    button.innerHTML = '<span class="spinner"></span>';
  }
  try {
    const result = await action();
    if (success) toast(typeof success === "function" ? success(result) : success);
    return result;
  } catch (err) {
    toast(err.message);
  } finally {
    if (button) {
      button.innerHTML = html;
      button.disabled = false;
    }
  }
}

export function openModal(id, focusSelector) {
  $(`#${id}`).classList.add("open");
  if (focusSelector) setTimeout(() => $(focusSelector)?.focus(), 30);
}

export function closeModal(id) {
  $(`#${id}`).classList.remove("open");
}

/** Close dismissable modals on backdrop click or a [data-close] button. */
export function initModals() {
  for (const modal of $$(".modal")) {
    modal.addEventListener("click", (e) => {
      const dismissable = modal.id !== "login-modal";
      if (dismissable && (e.target === modal || e.target.closest("[data-close]"))) modal.classList.remove("open");
    });
  }
}

/** Set text and an indicator dot class for a stat cell. */
export function setStat(id, text, tone = "") {
  $(`#${id}`).textContent = text;
  $(`#${id}-dot`).className = `dot ${tone}`;
}
