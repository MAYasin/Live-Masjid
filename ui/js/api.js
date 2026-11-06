// JSON API client. GET when no body is given, POST otherwise.

let onUnauthorized = () => {};

/** Register what to do when the server asks for a login (HTTP 401). */
export function setUnauthorizedHandler(handler) {
  onUnauthorized = handler;
}

export async function api(path, body) {
  const options = body === undefined
    ? {}
    : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  const res = await fetch(`/api/${path}`, options);
  if (res.status === 401) {
    onUnauthorized();
    throw new Error("Sign-in required");
  }
  const text = await res.text();
  let data;
  try { data = JSON.parse(text); } catch { data = text; }
  if (!res.ok) throw new Error((typeof data === "string" && data) || data?.message || "Request failed");
  return data;
}
