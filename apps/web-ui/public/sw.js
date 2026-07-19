/* Minimal service worker: runtime-cache the app shell + hashed assets for
 * installability/offline UI. API calls are never cached — the kernel is the
 * source of truth (ADR 0004). Vite hashes /assets/*, so runtime caching (not
 * a static precache list) keeps the cache correct across builds. */
const CACHE = "kelter-shell-v2";

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))
    )
  );
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET") return;
  if (url.pathname.includes("/api/") || url.pathname.startsWith("/a2a")) return; // network only
  e.respondWith(
    caches.match(e.request).then(
      (hit) =>
        hit ||
        fetch(e.request).then((res) => {
          if (res.ok && url.origin === location.origin) {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(e.request, copy));
          }
          return res;
        })
    )
  );
});
