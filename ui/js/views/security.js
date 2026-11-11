// Settings › Password: protect the page on the network, and sign out.

import { api } from "../api.js";
import { $, closeModal, openModal, runAction, toast } from "../dom.js";
import { subscribe } from "../store.js";

export function init() {
  $("#set-password").addEventListener("click", () => {
    $("#new-password").value = "";
    openModal("password-modal", "#new-password");
  });
  $("#password-form").addEventListener("submit", savePassword);
  $("#remove-password").addEventListener("click", removePassword);
  $("#logout").addEventListener("click", async () => {
    await api("logout", {}).catch(() => {});
    location.reload();
  });
  subscribe(render);
}

export function show() {}

function render(s) {
  $("#password-info").textContent = s.password_set ? "This page is password protected" : "Anyone on your network can open this page";
  $("#set-password").textContent = s.password_set ? "Change password" : "Set password";
  $("#logout-row").hidden = !s.password_set;
  $("#remove-password").hidden = !s.password_set;
}

/** Remove the password: anyone on the network can open the page again. */
async function removePassword(e) {
  if (!confirm("Remove the password? Anyone on your network will be able to open this page.")) return;
  runAction(e.currentTarget, () => api("password", { new_password: "" }), "Password removed");
}

async function savePassword(e) {
  e.preventDefault();
  const value = $("#new-password").value;
  try {
    await api("password", { new_password: value });
    closeModal("password-modal");
    toast(value ? "Password saved" : "Password removed");
  } catch (err) {
    toast(err.message);
  }
}
