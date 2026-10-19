const APP = "http://127.0.0.1:47219/add"; // keep in sync with browser::PORT

chrome.downloads.onCreated.addListener(async (item) => {
  const url = item.finalUrl || item.url;
  if (!/^https?:/.test(url)) return; // blob:/data: downloads stay in the browser
  try {
    const cookies = await chrome.cookies.getAll({ url });
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
  await chrome.downloads.cancel(item.id);
  chrome.downloads.erase({ id: item.id });
});
