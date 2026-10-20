// Runs in Chrome/Edge (service worker) and Firefox (background script).
const api = globalThis.browser ?? chrome; // Firefox's `browser` returns promises everywhere
const APP = "http://127.0.0.1:47219/add"; // keep in sync with browser::PORT

api.downloads.onCreated.addListener(async (item) => {
  const url = item.finalUrl || item.url; // Firefox has no finalUrl
  if (!/^https?:/.test(url)) return; // blob:/data: downloads stay in the browser
  try {
    const cookies = await api.cookies.getAll({ url });
    const res = await fetch(APP, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        url,
        referrer: item.referrer || null,
        cookies: cookies.map((c) => `${c.name}=${c.value}`).join("; ") || null,
        user_agent: navigator.userAgent,
      }),
    });
    if (!res.ok) return;
  } catch {
    return; // app not running: the browser keeps the download
  }
  await api.downloads.cancel(item.id);
  api.downloads.erase({ id: item.id });
});
