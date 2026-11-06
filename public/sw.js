// Cache name must change whenever the caching strategy changes: activate
// deletes every other cache, which is how clients stuck on an old strategy
// (e.g. pes-v1's cache-first HTML) get unpinned.
// Keep the precache tiny so updates install fast (a multi-MB shell would
// delay activation, and the old worker keeps serving stale pages until then).
// MediaPipe vendor files are still cached lazily by the fetch handler on
// first use.
// v5: the pixel worker is content-hashed (run.<hash>.js imports
// pixel_worker.<hash>.js, which loads pixel_worker_bg.<hash>.wasm), so it is
// cache-first like the other fingerprinted assets instead of network-first.
const CACHE = 'pes-v6';
const SHELL = [
  './', './index.html', './manifest.webmanifest', './icon-192.png', './icon-512.png',
];

// Drive grid/loupe thumbnails. They are cross-origin <img> loads, so the
// responses are opaque: no Content-Length to budget by, hence a count cap
// (600 entries is roughly 200 MB at Drive's usual w1600 sizes). Kept separate
// from the shell cache so the LRU trim never touches app files, and exempt
// from the activate sweep for the same reason.
const THUMB_CACHE = 'pes-drive-thumbs-v1';
const THUMB_MAX = 600;

function isDriveThumb(url) {
  return url.startsWith('https://drive.google.com/thumbnail?');
}

async function trimThumbs(cache) {
  const keys = await cache.keys();
  if (keys.length <= THUMB_MAX) return;
  // keys() is insertion order; a hit re-inserts its entry, so this drops the
  // least recently served thumbnails.
  await Promise.all(keys.slice(0, keys.length - THUMB_MAX).map((k) => cache.delete(k)));
}

async function driveThumb(e) {
  const cache = await caches.open(THUMB_CACHE);
  const hit = await cache.match(e.request);
  if (hit) {
    e.waitUntil(
      cache.delete(e.request)
        .then(() => cache.put(e.request, hit.clone()))
        .then(() => trimThumbs(cache))
    );
    return hit;
  }
  try {
    const res = await fetch(e.request);
    if (res.ok || res.type === 'opaque') {
      e.waitUntil(cache.put(e.request, res.clone()).then(() => trimThumbs(cache)));
    }
    return res;
  } catch {
    return Response.error();
  }
}

// trunk fingerprints these with a 16-hex hash; anything matching that wasn't
// referenced by the current index.html belongs to a previous deploy.
const HASHED_ASSET = /(photo-edit-simplified-[a-f0-9]{16}(?:_bg)?\.(?:js|wasm)|style-[a-f0-9]{16}\.css)/g;
const HASHED_NAME = /^photo-edit-simplified-[a-f0-9]{16}(?:_bg)?\.(?:js|wasm)$|^style-[a-f0-9]{16}\.css$/;
// build.sh hashes the worker bundle the same way.
const WORKER_NAME = /^(?:run|pixel_worker|pixel_worker_bg)\.[a-f0-9]{16}\.(?:js|wasm)$/;

// Hashed worker files referenced by the current index.html, found by following
// meta -> run.js -> glue -> wasm. Null when the chain can't be read (offline):
// keep everything and prune on a later activate.
async function liveWorkerFiles(html) {
  const m = html.match(/<meta name="pes-worker" content="([^"]+)"/);
  if (!m) return null;
  const at = (name) => new URL(name, self.registration.scope).href;
  try {
    const run = at(m[1]);
    const keep = new Set([run]);
    const runSrc = await (await fetch(run, { cache: 'no-cache' })).text();
    const glue = runSrc.match(/pixel_worker\.[a-f0-9]{16}\.js/);
    if (!glue) return null;
    const glueUrl = at('pixel-worker/' + glue[0]);
    keep.add(glueUrl);
    const glueSrc = await (await fetch(glueUrl, { cache: 'no-cache' })).text();
    const wasm = glueSrc.match(/pixel_worker_bg\.[a-f0-9]{16}\.wasm/);
    if (!wasm) return null;
    keep.add(at('pixel-worker/' + wasm[0]));
    return keep;
  } catch {
    return null;
  }
}

// Model files live under models/mobilesam/<version>/, which is not trunk-
// hashed; the version pointer decides what to keep. A failed read (offline
// activate) keeps everything so an offline session doesn't lose its model.
async function pruneStaleModels(cache) {
  const current = await modelVersion(cache);
  if (!current) return;
  const keys = await cache.keys();
  await Promise.all(keys.map((req) => {
    const m = new URL(req.url).pathname.match(/\/models\/mobilesam\/([^/]+)\//);
    if (m && m[1] !== current) return cache.delete(req);
    return undefined;
  }));
}

async function modelVersion(cache) {
  try {
    const res = await fetch('./models/mobilesam/current.json', { cache: 'no-cache' });
    if (res.ok) return (await res.json()).version || null;
  } catch {}
  try {
    const hit = await cache.match('./models/mobilesam/current.json');
    if (hit) return (await hit.json()).version || null;
  } catch {}
  return null;
}

async function pruneStaleAssets() {
  let html = '';
  try {
    html = await (await fetch('./index.html', { cache: 'no-cache' })).text();
  } catch {
    return; // offline during activate: keep everything, prune next time
  }
  const live = new Set(html.match(HASHED_ASSET) || []);
  const liveWorker = await liveWorkerFiles(html);
  const cache = await caches.open(CACHE);
  const keys = await cache.keys();
  await Promise.all(keys.map((req) => {
    const path = new URL(req.url).pathname;
    const name = path.slice(path.lastIndexOf('/') + 1);
    if (HASHED_NAME.test(name) && !live.has(name)) {
      return cache.delete(req);
    }
    if (liveWorker && path.includes('/pixel-worker/') && WORKER_NAME.test(name) && !liveWorker.has(req.url)) {
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
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE && k !== THUMB_CACHE).map((k) => caches.delete(k))))
      .then(() => pruneStaleAssets())
      .then(() => caches.open(CACHE))
      .then((c) => pruneStaleModels(c))
      .then(() => self.clients.claim())
      // Tell open pages to reload once so they pick up the fresh HTML/JS that
      // the old cache-first worker was pinning. index.html listens for this.
      .then(() => self.clients.matchAll())
      .then((clients) => clients.forEach((c) => c.postMessage('pes-reload')))
  );
});

self.addEventListener('fetch', (e) => {
  if (e.request.method !== 'GET') return;
  if (isDriveThumb(e.request.url)) {
    e.respondWith(driveThumb(e));
    return;
  }
  if (!e.request.url.startsWith(self.location.origin)) return;

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

  // Un-hashed control files: a deploy must never be pinned behind the cache.
  const path = new URL(e.request.url).pathname;
  if (path.endsWith('/mobilesam/worker.js') || path.endsWith('/models/mobilesam/current.json')) {
    e.respondWith(
      fetch(e.request).then((res) => {
        if (res.ok) {
          const copy = res.clone();
          e.waitUntil(caches.open(CACHE).then((c) => c.put(e.request, copy)));
        }
        return res;
      }).catch(() => caches.match(e.request).then((hit) => hit || Response.error()))
    );
    return;
  }

  // Everything else (hashed JS/WASM, the fingerprinted pixel worker, fonts,
  // vendor models): cache-first. A new deploy changes the hashes, and
  // pruneStaleAssets drops the old files on activate.
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
