/* Minimal service worker: cache the app shell for installability/offline UI.
 * API calls are never cached — the kernel is the source of truth. */
const CACHE = "ak-shell-v1";
const SHELL = ["./", "index.html", "style.css", "app.js", "manifest.webmanifest", "icon.svg"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))
    )
  );
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (url.pathname.includes("/api/")) return; // network only
  e.respondWith(
    caches.match(e.request).then((hit) => hit || fetch(e.request))
  );
});
