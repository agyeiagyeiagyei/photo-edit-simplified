const CACHE = 'pes-v1';
const SHELL = [
  './', './index.html', './manifest.webmanifest', './icon-192.png', './icon-512.png',
  './vendor/mediapipe-tasks-vision/vision_bundle.mjs',
  './vendor/mediapipe-tasks-vision/wasm/vision_wasm_internal.js',
  './vendor/mediapipe-tasks-vision/wasm/vision_wasm_internal.wasm',
  './vendor/mediapipe-tasks-vision/wasm/vision_wasm_nosimd_internal.js',
  './vendor/mediapipe-tasks-vision/wasm/vision_wasm_nosimd_internal.wasm',
  './vendor/mediapipe-selfie-segmenter/selfie_segmenter.tflite',
];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (e) => {
  if (e.request.method !== 'GET' || !e.request.url.startsWith(self.location.origin)) return;
  e.respondWith(
    caches.match(e.request).then((hit) => hit || fetch(e.request).then((res) => {
      const copy = res.clone();
      caches.open(CACHE).then((c) => c.put(e.request, copy));
      return res;
    }))
  );
});
