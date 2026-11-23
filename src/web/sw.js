"use strict";

// Keeps a copy of the console's shell (page, styles, scripts, icons) so the
// installed app opens offline. Console data under /v1/ and anything that is not a
// plain same-origin GET always goes to the network and is never stored.
const CACHE = "rworker-shell-1";
const SHELL = [
  "/",
  "/assets/app.css",
  "/assets/app.js",
  "/assets/xterm.css",
  "/assets/xterm.js",
  "/assets/xterm-fit.js",
  "/assets/favicon.svg",
  "/assets/logo.svg",
  "/assets/agents.woff2?v=5",
  "/assets/icon-192.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).catch(() => {}));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const key of await caches.keys()) if (key !== CACHE) await caches.delete(key);
    await self.clients.claim();
  })());
});

// Store only complete answers from this origin, never a sign-in redirect.
function storable(response) {
  return response.ok && !response.redirected && response.type === "basic";
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin || url.pathname.startsWith("/v1/")) return;
  if (request.mode === "navigate") {
    // The page itself: network first, so a new version or a sign-in shows
    // at once; the stored copy only when the network is down.
    event.respondWith((async () => {
      try {
        const response = await fetch(request);
        if (storable(response)) (await caches.open(CACHE)).put("/", response.clone());
        return response;
      } catch {
        return (await caches.match("/")) || Response.error();
      }
    })());
    return;
  }
  if (!url.pathname.startsWith("/assets/")) return;
  // Assets: network first as well, so a new page never runs against old
  // scripts after a deploy; the stored copy keeps the shell usable offline.
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const response = await fetch(request);
      if (storable(response)) cache.put(request, response.clone());
      return response;
    } catch {
      return (await cache.match(request)) || Response.error();
    }
  })());
});
