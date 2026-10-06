// Cache name must change whenever the caching strategy changes: activate
// deletes every other cache, which is how clients stuck on an old strategy
// (e.g. pes-v1's cache-first HTML) get unpinned.
// Keep the precache tiny so updates install fast (a multi-MB shell would
// delay activation, and the old worker keeps serving stale pages until then).
// MediaPipe vendor files are still cached lazily by the fetch handler on
// first use.
const CACHE = 'pes-v3';
const SHELL = [
  './', './index.html', './manifest.webmanifest', './icon-192.png', './icon-512.png',
];

// trunk fingerprints these with a 16-hex hash; anything matching that wasn't
// referenced by the current index.html belongs to a previous deploy.
const HASHED_ASSET = /(photo-edit-simplified-[a-f0-9]{16}(?:_bg)?\.(?:js|wasm)|style-[a-f0-9]{16}\.css)/g;
const HASHED_NAME = /^photo-edit-simplified-[a-f0-9]{16}(?:_bg)?\.(?:js|wasm)$|^style-[a-f0-9]{16}\.css$/;

async function pruneStaleAssets() {
  let html = '';
  try {
    html = await (await fetch('./index.html', { cache: 'no-cache' })).text();
  } catch {
    return; // offline during activate: keep everything, prune next time
  }
  const live = new Set(html.match(HASHED_ASSET) || []);
  const cache = await caches.open(CACHE);
  const keys = await cache.keys();
  await Promise.all(keys.map((req) => {
    const path = new URL(req.url).pathname;
    const name = path.slice(path.lastIndexOf('/') + 1);
    if (HASHED_NAME.test(name) && !live.has(name)) {
      return cache.delete(req);
    }
    return undefined;
  }));
}

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => pruneStaleAssets())
      .then(() => self.clients.claim())
      // Tell open pages to reload once so they pick up the fresh HTML/JS that
      // the old cache-first worker was pinning. index.html listens for this.
      .then(() => self.clients.matchAll())
      .then((clients) => clients.forEach((c) => c.postMessage('pes-reload')))
  );
});

self.addEventListener('fetch', (e) => {
  if (e.request.method !== 'GET' || !e.request.url.startsWith(self.location.origin)) return;

  // HTML/navigations: network-first so deploys reach users immediately;
  // fall back to cache offline.
  if (e.request.mode === 'navigate') {
    e.respondWith(
      fetch(e.request).then((res) => {
        if (res.ok) {
          const copy = res.clone();
          e.waitUntil(caches.open(CACHE).then((c) => c.put(e.request, copy)));
        }
        return res;
      }).catch(() => caches.match(e.request).then((hit) => hit || caches.match('./index.html')))
    );
    return;
  }

  // Everything else (hashed JS/WASM, fonts, vendor models): cache-first.
  e.respondWith(
    caches.match(e.request).then((hit) => hit || fetch(e.request).then((res) => {
      if (res.ok) {
        const copy = res.clone();
        e.waitUntil(caches.open(CACHE).then((c) => c.put(e.request, copy)));
      }
      return res;
    }))
  );
});
