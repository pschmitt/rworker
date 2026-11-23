"use strict";

const $ = (id) => document.getElementById(id);
const root = document.documentElement;
const SVG_NS = "http://www.w3.org/2000/svg";
const views = { workers: "Workers", jobs: "Jobs", events: "Events", profiles: "Profiles" };
const narrow = matchMedia("(max-width: 1180px)");
const leaseWarningSeconds = 15 * 60;

// Preferences: layout (split view or cards), theme and density, kept per browser.
const prefChoices = { layout: ["split", "cards", "console"], theme: ["system", "dark", "light"], density: ["comfortable", "compact"] };
const prefs = { layout: "split", theme: "system", density: "comfortable" };
try { Object.assign(prefs, JSON.parse(localStorage.getItem("rworker.prefs") || "{}")); } catch { /* Storage disabled. */ }
for (const [key, choices] of Object.entries(prefChoices)) if (!choices.includes(prefs[key])) prefs[key] = choices[0];

function applyPrefs() {
  root.dataset.theme = prefs.theme;
  root.dataset.layout = prefs.layout;
  root.dataset.density = prefs.density;
}

function setPref(key, value) {
  if (!prefChoices[key]?.includes(value)) return;
  prefs[key] = value;
  try { localStorage.setItem("rworker.prefs", JSON.stringify(prefs)); } catch { /* Storage disabled. */ }
  applyPrefs();
  for (const input of document.querySelectorAll(`#settings input[name="${key}"]`)) input.checked = input.value === value;
  lastRenderKey = "";
  detailPanel();
  render();
}
applyPrefs();

const state = {
  token: "", authenticated: false, view: "workers", rows: {}, selected: null, job: null, tab: "logs",
  filters: { workers: "live", jobs: "active", events: "all" }, sort: "newest",
  redact: true, follow: true, wrap: true, logQuery: "", logText: "", logRendered: "", logSgr: {}, limit: 200,
  detailJobs: null, raw: false, dismissed: new Set(), order: [],
};
let stream;
let logSocket;
let logRetryAt = 0;
let loading = false;
let detailRequest = 0;
let detailLoading = false;
let currentAction;
let lastRenderKey = "";
let lastHeadKey = "";
let lastSearchView = "";
const rowCache = new Map();

// DOM helpers
function el(tag, className, ...children) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const child of children) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function icon(name, extra = "") {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "i" + (extra ? " " + extra : ""));
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG_NS, "use");
  use.setAttribute("href", "#i-" + name);
  svg.append(use);
  return svg;
}

function button(label, onClick, { cls = "btn", ic, title, aria } = {}) {
  const node = el("button", cls);
  node.type = "button";
  if (ic) node.append(icon(ic));
  if (label) node.append(el("span", null, label));
  if (title) node.title = title;
  if (aria) node.setAttribute("aria-label", aria);
  if (onClick) node.addEventListener("click", onClick);
  return node;
}

function iconButton(ic, label, onClick, cls = "btn quiet sm icon") {
  return button(null, onClick, { cls, ic, title: label, aria: label });
}

// Toasts replace the old banner notices: they never move the page.
function toast(message, { tone = "ok", detail, actions = [], timeout = 5000 } = {}) {
  const node = el("div", "toast");
  let timer;
  const close = () => { clearTimeout(timer); node.remove(); };
  const arm = (ms) => { clearTimeout(timer); if (ms) timer = setTimeout(close, ms); };
  const build = (text, opts) => {
    const tone = opts.tone || "ok";
    node.className = "toast" + (tone === "error" ? " error" : tone === "busy" ? " busy" : "");
    node.setAttribute("role", tone === "error" ? "alert" : "status");
    const msg = el("div", "msg", text);
    if (opts.detail) msg.append(el("small", null, opts.detail));
    node.replaceChildren(icon(tone === "error" ? "alert" : tone === "busy" ? "loader" : tone === "info" ? "info" : "check", tone === "busy" ? "spin" : ""), msg);
    for (const action of opts.actions || []) node.append(button(action.label, () => { close(); action.run(); }, { cls: "btn quiet sm" }));
    node.append(iconButton("x", "Dismiss", close));
    arm(opts.timeout ?? (tone === "error" ? 9000 : tone === "busy" ? 0 : 5000));
  };
  build(message, { tone, detail, actions, timeout });
  node.addEventListener("mouseenter", () => clearTimeout(timer));
  node.addEventListener("mouseleave", () => { if (!node.classList.contains("busy")) arm(4000); });
  const stack = $("toasts");
  stack.append(node);
  while (stack.children.length > 4) stack.firstElementChild.remove();
  return { close, update: (text, opts = {}) => build(text, opts) };
}

function fail(message) {
  return toast(message, { tone: "error" });
}

function connection(label, kind = "") {
  if ($("connection").textContent !== label) $("connection").textContent = label;
  $("signal").className = "signal" + (kind ? " " + kind : "");
}

async function request(path, data) {
  const access = state.token;
  const response = await fetch("/v1/ui/" + path, {
    method: data ? "POST" : "GET",
    headers: {
      ...(access ? { Authorization: "Bearer " + access } : {}),
      ...(data ? { "Content-Type": "application/json" } : {}),
    },
    body: data ? JSON.stringify(data) : undefined,
    cache: "no-store",
    redirect: "manual",
  });
  if (response.type === "opaqueredirect") {
    throw new Error("Your Authelia session expired. Reload to sign in again.");
  }
  if (response.status === 401) {
    if (state.token === access) lock();
    throw new Error("Enter the console access token.");
  }
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || "The controller did not respond.");
  if (path === "workers") state.inventoryStale = Boolean(payload.stale);
  return payload.data;
}

function lock() {
  closeLogStream();
  for (const dialog of document.querySelectorAll("dialog[open]")) dialog.close();
  closeMenu();
  state.authenticated = false;
  state.token = "";
  try { sessionStorage.removeItem("rworker.operator"); } catch { /* Storage disabled. */ }
  state.rows = {};
  state.selected = null;
  rowCache.clear();
  lastRenderKey = "";
  detailRequest++;
  stream?.close();
  stream = undefined;
  $("console").hidden = true;
  $("toolbar").hidden = true;
  $("login").hidden = false;
  $("lock").hidden = true;
  $("sign-out").hidden = true;
  $("create").hidden = true;
  $("fab").hidden = true;
  $("results").replaceChildren();
  $("detail-output").replaceChildren();
  $("session-detail").textContent = "";
  connection("Locked");
}

// Routing: #workers, #workers/NAME, #workers/NAME/JOB, #jobs, #events, #profiles.
function navigate() {
  const [requested, worker, job] = location.hash.slice(1).split("/");
  const view = Object.hasOwn(views, requested) ? requested : "workers";
  if (view !== state.view) {
    state.limit = 200;
    $("search").value = "";
  }
  state.view = view;
  document.querySelectorAll("[data-view]").forEach((link) => {
    if (link.dataset.view === state.view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  $("search").placeholder = { workers: "Filter workers", jobs: "Filter jobs", events: "Filter events", profiles: "Filter profiles" }[view];
  if (state.authenticated && view === "workers" && worker) {
    openWorker(workerFor(decodeURIComponent(worker)), job ? decodeURIComponent(job) : null);
  } else {
    state.selected = null;
    state.job = null;
    detailPanel();
  }
  lastRenderKey = "";
  render();
  refresh();
}

// Glyphs from the bundled icon font (agents.woff2). Keep in sync with
// scripts/web-icon-font.sh, which builds the font from these.
const agentIcons = new Map([
  ["claude", ["", "claude"]], ["codex", ["", "codex"]],
  ["shell", ["", ""]], ["nixbot", ["", "nix"]], ["nix build", ["", "nix"]],
  ["copilot", ["", ""]], ["gemini", ["", ""]], ["agy", ["", ""]],
  ["circus", ["\u{f0508}", ""]],
  ["circus-qemu", ["\u{f0508}", ""]],
  ["antigravity", ["", ""]], ["browser-daemon", ["\u{f02a0}", ""]],
]);
const githubGlyph = "\u{f09b}";
const gitlabGlyph = "\u{f296}";
const ciProviders = { github: "GitHub Actions", gitlab: "GitLab CI" };
const openstackGlyph = "\u{e856}";
const workerGlyphs = new Set(["\u{f1064}", "\u{f06a9}", "\u{f048b}", "\u{f0508}", "\u{f059f}", githubGlyph]);

function isPool(row) {
  return Boolean(row.kind) && !["adhoc", "unknown"].includes(row.kind);
}

function workerGlyph(row) {
  if (row.name?.startsWith("gha-")) return githubGlyph;
  if (row.provider === "gitlab" || row.name?.startsWith("glci-")) return gitlabGlyph;
  if (row.icon && workerGlyphs.has(row.icon)) return row.icon;
  if (row.kind === "adhoc") return "\u{f1064}";
  if (row.kind === "nixbot" || row.kind === "pool") return "\u{f06a9}";
  return "\u{f048b}";
}

function profileGlyph(row) {
  if (row.provider === "github") return githubGlyph;
  if (row.provider === "gitlab") return gitlabGlyph;
  return openstackGlyph;
}

function providerKey(row) {
  return Object.hasOwn(ciProviders, row.provider) ? row.provider : "openstack";
}

function workerTile(row) {
  const glyph = workerGlyph(row);
  const kind = glyph === githubGlyph ? " github" : glyph === gitlabGlyph ? " gitlab" : isPool(row) ? " pool" : "";
  const tile = el("span", "tile" + kind + (row.retired ? " retired" : ""), glyph);
  tile.setAttribute("aria-hidden", "true");
  return tile;
}

function profileTile(row) {
  const tile = el("span", "tile " + providerKey(row), profileGlyph(row));
  tile.setAttribute("aria-hidden", "true");
  return tile;
}

function glyph(char, cls = "") {
  const node = el("span", "glyph" + (cls ? " " + cls : ""), char);
  node.setAttribute("aria-hidden", "true");
  return node;
}

function agentLabel(agent, agentId) {
  if (!agent) return el("span", "muted", "—");
  const label = el("span");
  const known = agentIcons.get(agent);
  const wrap = el("span", null);
  wrap.append(...(known ? [glyph(known[0], known[1]), " "] : []), agent);
  if (agentId) wrap.append(el("span", "muted", " · " + agentId));
  label.append(wrap);
  return label;
}

// Formatting
function duration(seconds) {
  seconds = Math.max(0, Math.floor(seconds));
  if (seconds < 60) return "<1m";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return minutes + "m";
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours + "h" + (minutes % 60 ? " " + (minutes % 60) + "m" : "");
  return Math.floor(hours / 24) + "d" + (hours % 24 ? " " + (hours % 24) + "h" : "");
}

function timestamp(value) {
  return typeof value === "number" ? value : Date.parse(value) / 1000;
}

function relative(value, now = Date.now() / 1000) {
  const seconds = now - timestamp(value);
  if (!Number.isFinite(seconds)) return "";
  if (seconds < 45) return "just now";
  return duration(seconds) + " ago";
}

function clock(value) {
  if (!value) return "—";
  const date = new Date(typeof value === "number" ? value * 1000 : value);
  return Number.isNaN(date.valueOf()) ? "—" : date.toLocaleString();
}

function timeOfDay(value) {
  const date = new Date(timestamp(value) * 1000);
  return Number.isNaN(date.valueOf()) ? "" : date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function dayLabel(value) {
  const date = new Date(timestamp(value) * 1000);
  const today = new Date();
  const start = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).valueOf();
  const days = Math.round((start(today) - start(date)) / 864e5);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return date.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" });
}

function leaseInfo(row, now) {
  if (row.retired) return { kind: "retired", text: "retired", long: "Retired", tone: "", pct: 0 };
  if (isPool(row) && row.pool_state) {
    const busy = row.pool_state === "busy";
    const idleFor = !busy && row.last_busy > 0 ? duration(now - row.last_busy) : "";
    return { kind: "pool", text: row.pool_state + (idleFor ? " " + idleFor : ""), long: busy ? "Busy" : "Idle" + (idleFor ? " for " + idleFor : ""), tone: busy ? "busy" : "idle", pct: 100 };
  }
  if (row.grace_until > now) {
    const left = row.grace_until - now;
    return { kind: "lease", text: "grace " + duration(left), long: "In its grace period, " + duration(left) + " left", tone: "urgent", pct: 100 };
  }
  if (row.expires_at > 0) {
    const left = row.expires_at - now;
    const created = timestamp(row.created_at);
    const total = Number.isFinite(created) ? row.expires_at - created : 0;
    const pct = total > 0 ? Math.min(100, Math.max(0, left / total * 100)) : 100;
    const tone = left < 0 ? "urgent" : left < leaseWarningSeconds ? "soon" : "";
    return {
      kind: "lease", pct, tone,
      text: left < 0 ? "overdue " + duration(-left) : duration(left) + " left",
      long: left < 0 ? "Overdue by " + duration(-left) : duration(left) + " left · expires " + timeOfDay(row.expires_at),
    };
  }
  return { kind: "none", text: "—", long: "No lease", tone: "", pct: 0 };
}

function leaseBar(lease, wide = false) {
  const bar = el("span", "bar" + (lease.kind === "pool" ? " pool-" + lease.tone : lease.tone ? " " + lease.tone : ""));
  const fill = el("i");
  fill.style.setProperty("--pct", Math.round(lease.pct) + "%");
  bar.append(fill);
  if (wide) bar.style.width = "100%";
  return bar;
}

function statusPill(row) {
  if (row.retired) return el("span", "pill", "Retired");
  const text = String(row.status || "Unknown");
  const key = text.toLowerCase();
  if (isPool(row) && row.pool_state && key === "active") {
    return el("span", "pill " + (row.pool_state === "busy" ? "busy" : ""), row.pool_state === "busy" ? "Busy" : "Idle");
  }
  const tone = /^(active|ready|running)$/.test(key) ? "good" : /^(build|boot|reboot|deleting|pending)/.test(key) ? "busy" : /^(error|failed)/.test(key) ? "bad" : "";
  return el("span", "pill" + (tone ? " " + tone : ""), text.charAt(0) + text.slice(1).toLowerCase());
}

function jobTone(value) {
  const key = String(value || "").toLowerCase();
  if (key === "running") return ["busy", "play"];
  if (key === "exit 0") return ["good", "check"];
  if (/^exit /.test(key) || /error|failed/.test(key)) return ["bad", "x"];
  return ["", "file"];
}

function stripAnsi(value) {
  return value.replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "");
}

function emptyState(ic, message, extra) {
  const node = el("div", "empty", icon(ic, ic === "loader" ? "spin" : ""), el("p", null, message));
  if (extra) node.append(extra);
  return node;
}

// Filters per view.
const viewFilters = {
  workers: [["live", "Live"], ["pool", "Pools"], ["expiring", "Expiring"], ["retired", "Retired"]],
  jobs: [["active", "Active"], ["all", "All"]],
  events: [["all", "All", "activity"], ["workers", "Workers", "server"], ["jobs", "Jobs", "jobs"], ["state", "State changes", "swap"], ["failures", "Failures", "alert"]],
  profiles: [],
};
const filterTests = {
  workers: {
    live: (row) => !row.retired,
    pool: (row) => !row.retired && isPool(row),
    expiring: (row, now) => !row.retired && !(isPool(row) && row.pool_state) &&
      (row.grace_until > now || (row.expires_at > 0 && row.expires_at - now < leaseWarningSeconds)),
    retired: (row) => Boolean(row.retired),
  },
  jobs: {
    active: (row) => row.state !== "archived" && !String(row.state).startsWith("exit "),
    all: () => true,
  },
  events: {
    all: () => true,
    workers: (row) => row.kind.startsWith("worker-"),
    jobs: (row) => row.kind.startsWith("job-"),
    state: (row) => row.kind.endsWith("-state"),
    failures: (row) => /exit [1-9]|error|failed|failure/i.test(row.details || ""),
  },
};

function renderFilters(now) {
  const host = $("filters");
  const defs = viewFilters[state.view];
  const source = state.rows[state.view];
  const key = JSON.stringify([state.view, state.filters[state.view], state.sort, source?.length, source && defs.map(([id]) => source.filter((row) => filterTests[state.view][id](row, now)).length)]);
  if (host.dataset.key === key) return;
  host.dataset.key = key;
  host.replaceChildren();
  for (const [id, label, ic] of defs) {
    const chip = el("button", "chip");
    chip.type = "button";
    if (ic && state.view === "events") chip.append(icon(ic));
    chip.append(label);
    if (source && state.view !== "events") chip.append(" ", el("b", null, source.filter((row) => filterTests[state.view][id](row, now)).length));
    chip.setAttribute("aria-pressed", String(state.filters[state.view] === id));
    chip.addEventListener("click", () => {
      state.filters[state.view] = id;
      state.limit = 200;
      render();
    });
    host.append(chip);
  }
  if (state.view === "workers") {
    const sort = el("select");
    sort.setAttribute("aria-label", "Sort workers");
    for (const [value, label] of [["newest", "Newest first"], ["lease", "Expiring first"], ["name", "Name"]]) {
      const option = el("option", null, label);
      option.value = value;
      sort.append(option);
    }
    sort.value = state.sort;
    sort.addEventListener("change", () => { state.sort = sort.value; render(); });
    host.append(sort);
  }
}

function renderNav(now) {
  const workers = state.rows.workers;
  const jobs = state.rows.jobs;
  const set = (key, text, cls = "") => {
    for (const node of document.querySelectorAll(`[data-count="${key}"]`)) {
      node.textContent = text;
      node.className = "count lbl" + (cls ? " " + cls : "");
    }
  };
  set("workers", workers ? String(workers.filter((row) => !row.retired).length) : "");
  const running = jobs ? jobs.filter((row) => filterTests.jobs.active(row, now)).length : 0;
  set("jobs", running ? running + " running" : "", "busy");
  set("profiles", state.rows.profiles ? String(state.rows.profiles.length) : "");
}

// Expiry banner: a lease about to run out, with a one-click extension.
function renderBanner(now) {
  const banner = $("banner");
  const fullPage = state.selected && !$("workspace").classList.contains("split");
  const expiring = state.view !== "workers" || fullPage ? [] : (state.rows.workers || []).filter((row) =>
    filterTests.workers.expiring(row, now) && !state.dismissed.has(row.name));
  const key = "k:" + expiring.map((row) => row.name + leaseInfo(row, now).text).join("|");
  if (banner.dataset.key === key) return;
  banner.dataset.key = key;
  banner.hidden = !expiring.length;
  if (!expiring.length) return;
  const [row] = expiring;
  const lease = leaseInfo(row, now);
  const link = el("a", "mono", row.name);
  link.href = workerHash(row);
  const text = el("span", null, link, " ", row.grace_until > now ? "is in its grace period" : lease.text.startsWith("overdue") ? "is overdue" : "expires in " + lease.text.replace(" left", ""),
    row.topic ? el("span", "muted", " · " + row.topic) : null,
    expiring.length > 1 ? el("span", "muted", ` · and ${expiring.length - 1} more`) : null);
  const actions = el("span", "actions",
    button("1h from now", () => runAction({ action: "extend", worker: row.name, ttl: "1h" }, `${row.name} now expires in 1 hour.`), { cls: "btn sm primary", ic: "clock" }),
    button("Extend…", () => form("extend", row), { cls: "btn sm" }),
    iconButton("x", "Dismiss", () => { state.dismissed.add(row.name); banner.dataset.key = ""; renderBanner(Date.now() / 1000); }));
  banner.replaceChildren(icon("alert"), text, actions);
}

function searchMatch(row, search) {
  return !search || JSON.stringify(row).toLowerCase().includes(search);
}

function render() {
  const now = Date.now() / 1000;
  if (!state.authenticated) return;
  renderNav(now);
  renderFilters(now);
  renderBanner(now);
  const view = state.view;
  const source = state.rows[view];
  const results = $("results");
  const search = $("search").value.trim().toLowerCase();
  if (!source) {
    if (lastRenderKey !== "loading:" + view) results.replaceChildren(emptyState("loader", "Loading " + view + "…"));
    lastRenderKey = "loading:" + view;
    return;
  }
  const test = filterTests[view]?.[state.filters[view]] || (() => true);
  let visible = source.filter((row) => searchMatch(row, search) && test(row, now));
  if (view === "events") visible = visible.slice().reverse();
  const key = JSON.stringify([view, prefs.layout, state.filters[view], state.sort, search, state.limit, state.selected?.name, Math.floor(now / 20), visible]);
  if (key === lastRenderKey) return;
  lastRenderKey = key;
  if (!visible.length) {
    const messages = {
      workers: { live: "No live workers.", pool: "No live pool workers.", expiring: "No leases expire in the next 15 minutes.", retired: "No retired workers." }[state.filters.workers],
      jobs: state.filters.jobs === "active" && source.length ? "No active jobs. Choose All to see earlier runs." : "No jobs yet.",
      events: "No events match.",
      profiles: "No profiles configured.",
    };
    const extra = view === "workers" && state.filters.workers === "live" && !search ? button("New worker", () => form("create"), { cls: "btn primary", ic: "plus" }) : null;
    results.replaceChildren(emptyState(search ? "search" : view === "workers" ? "server" : "info", search ? "Nothing matches your filter." : messages[view], extra));
    state.order = [];
    return;
  }
  const scroll = $("inventory").scrollTop;
  switch (view) {
    case "workers":
      if (prefs.layout === "cards") renderWorkerCards(visible, now);
      else if (prefs.layout === "console" && !narrow.matches) renderWorkerTable(visible, now);
      else renderWorkerList(visible, now);
      break;
    case "jobs": renderJobs(visible); break;
    case "events": renderEvents(visible, now); break;
    default: renderProfiles(visible);
  }
  $("inventory").scrollTop = scroll;
}

function showMore(container, total) {
  if (total <= state.limit) return;
  const more = el("div", "more-row", `Showing ${state.limit} of ${total} · `,
    button("Show more", () => { state.limit += 200; render(); }, { cls: "" }));
  container.append(more);
}

function sortWorkers(rows, now) {
  const sorted = rows.slice();
  if (state.sort === "name") sorted.sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));
  else if (state.sort === "lease") {
    const left = (row) => isPool(row) ? Infinity : row.grace_until > now ? row.grace_until - now : row.expires_at > 0 ? row.expires_at - now : Infinity;
    sorted.sort((a, b) => left(a) - left(b));
  } else sorted.sort((a, b) => (timestamp(b.created_at) || 0) - (timestamp(a.created_at) || 0));
  return sorted;
}

function groupWorkers(rows, now) {
  rows = sortWorkers(rows, now);
  if (state.filters.workers === "retired" || state.filters.workers === "expiring") return [[null, null, rows]];
  const adhoc = rows.filter((row) => !isPool(row));
  const pools = new Map();
  for (const row of rows.filter(isPool)) {
    const key = row.pool || row.kind;
    if (!pools.has(key)) pools.set(key, []);
    pools.get(key).push(row);
  }
  const groups = [];
  if (adhoc.length) groups.push(["Ad-hoc", "cloud", adhoc]);
  for (const [name, members] of pools) groups.push([name + " pool", "tent", members]);
  return groups;
}

function workerSub(row) {
  return row.topic || row.label || (isPool(row) ? row.kind + " pool" : row.owner) || "";
}

function workerRow(row, now) {
  const lease = leaseInfo(row, now);
  const data = JSON.stringify([row.topic, row.label, row.status, row.retired, lease.text, lease.tone, Math.round(lease.pct / 5), row.icon]);
  const current = state.selected?.name === row.name;
  const cached = rowCache.get(row.name);
  if (cached && cached.data === data) {
    if (current) cached.node.setAttribute("aria-current", "true");
    else cached.node.removeAttribute("aria-current");
    return cached.node;
  }
  const node = el("a", "row worker-item");
  node.href = workerHash(row);
  node.dataset.worker = row.name;
  if (current) node.setAttribute("aria-current", "true");
  const right = lease.kind === "pool" ? el("span", "pill" + (lease.tone === "busy" ? " busy" : ""), lease.text) :
    lease.kind === "retired" ? el("span", "lease-text", relative(row.created_at, now)) :
      el("span", "lease", el("span", "lease-text" + (lease.tone ? " " + lease.tone : ""), lease.text), leaseBar(lease));
  node.append(workerTile(row), el("span", "ident", el("span", "name", row.name), el("span", "sub", workerSub(row))), right);
  if (cached) node.classList.add("flash");
  rowCache.set(row.name, { node, data });
  return node;
}

function renderWorkerList(visible, now) {
  const container = el("div", "worker-list");
  const order = [];
  let shown = 0;
  for (const [title, ic, rows] of groupWorkers(visible, now)) {
    if (title) container.append(el("div", "group", icon(ic), title, " ", el("span", null, rows.length)));
    for (const row of rows) {
      if (shown++ >= state.limit) break;
      container.append(workerRow(row, now));
      order.push(row.name);
    }
  }
  showMore(container, visible.length);
  state.order = order;
  $("results").replaceChildren(container);
}

const tableCache = new Map();
function tableRow(row, now) {
  const lease = leaseInfo(row, now);
  const data = JSON.stringify([row.topic, row.agent, row.status, row.retired, lease.text, lease.tone, Math.round(lease.pct / 5)]);
  const current = state.selected?.name === row.name;
  const cached = tableCache.get(row.name);
  if (cached && cached.data === data) {
    if (current) cached.node.setAttribute("aria-current", "true");
    else cached.node.removeAttribute("aria-current");
    return cached.node;
  }
  const status = row.retired ? ["retired", "idle"] : lease.kind === "pool" ? [row.pool_state, lease.tone === "busy" ? "busy" : "idle"] :
    [String(row.status || "?").toLowerCase(), /^(active|ready)$/i.test(row.status) ? "good" : /^(build|boot|deleting|pending)/i.test(row.status) ? "busy" : "bad"];
  const kind = row.name.startsWith("gha-") ? "github" : row.provider === "gitlab" || row.name.startsWith("glci-") ? "gitlab" : isPool(row) ? (row.pool || row.kind) : row.kind || "";
  const glyphChar = workerGlyph(row);
  const glyphClass = glyphChar === githubGlyph ? "github" : glyphChar === gitlabGlyph ? "gitlab" : isPool(row) ? "pool" : "adhoc";
  const node = el("a", "ctr worker-item", el("span", "mark"), el("span", "nm", row.name), el("span", "kd", glyph(glyphChar, glyphClass), el("span", "muted", kind)), el("span", "st-" + status[1], status[0]),
    el("span", null, row.agent || "—"), el("span", "tp", row.topic || ""),
    el("span", "ls", leaseBar(lease), el("span", "lease-text" + (lease.tone && lease.kind !== "pool" ? " " + lease.tone : ""), lease.kind === "pool" ? lease.text : lease.text.replace(" left", ""))),
    el("span", "muted", relative(row.created_at, now).replace(" ago", "")));
  node.href = workerHash(row);
  node.dataset.worker = row.name;
  if (current) node.setAttribute("aria-current", "true");
  if (cached) node.classList.add("flash");
  tableCache.set(row.name, { node, data });
  return node;
}

function renderWorkerTable(visible, now) {
  const table = el("div", "ctable");
  table.setAttribute("role", "list");
  table.append(el("div", "ctr head", el("span"), el("span", null, "NAME"), el("span", null, "KIND"), el("span", null, "STATUS"), el("span", null, "AGENT"), el("span", null, "TOPIC"), el("span", null, "LEASE"), el("span", null, "AGE")));
  const order = [];
  for (const row of sortWorkers(visible, now).slice(0, state.limit)) {
    table.append(tableRow(row, now));
    order.push(row.name);
  }
  showMore(table, visible.length);
  state.order = order;
  $("results").replaceChildren(table);
}

function workerMenuButton(row, cls = "btn sm icon quiet") {
  const trigger = iconButton("more", "More actions", (event) => { event.preventDefault(); openMenu(trigger, workerMenu(row)); }, cls);
  return trigger;
}

function renderWorkerCards(visible, now) {
  const container = el("div");
  const sorted = sortWorkers(visible, now);
  const single = sorted.filter((row) => !isPool(row) || row.retired);
  const order = [];
  if (single.length) {
    if (state.filters.workers !== "retired") container.append(el("div", "group", icon("cloud"), "Ad-hoc ", el("span", null, single.length)));
    const grid = el("div", "cards");
    for (const row of single.slice(0, state.limit)) {
      const lease = leaseInfo(row, now);
      const card = el("article", "card");
      card.dataset.worker = row.name;
      const name = el("a", "name", row.name);
      name.href = workerHash(row);
      const profile = state.rows.profiles?.find((item) => item.name === row.profile);
      const facts = el("div", "card-facts",
        el("span", null, icon("cpu"), [row.profile, profile?.vcpus ? profile.vcpus + " vCPU" : ""].filter(Boolean).join(" · ") || "—"),
        el("span", null, agentLabel(row.agent)),
        el("span", null, icon("user"), row.owner || "—"),
        el("span", null, icon("clock"), relative(row.created_at, now).replace(" ago", " old")));
      card.append(el("div", "card-head", workerTile(row), el("div", "ident", name, el("span", "sub", workerSub(row))), statusPill(row)), facts);
      if (lease.kind === "lease") {
        card.append(el("div", "card-lease", el("div", null, el("span", null, "Lease"), el("span", "lease-text" + (lease.tone ? " " + lease.tone : ""), lease.text)), leaseBar(lease, true)));
      }
      const actions = el("div", "card-actions");
      if (!row.retired) actions.append(button("SSH", () => openTerminal(row), { cls: "btn sm", ic: "terminal" }));
      const open = el("a", "btn sm", icon("file"), el("span", null, row.retired ? "Logs" : "Open"));
      open.href = workerHash(row);
      actions.append(open);
      if (!row.retired) actions.append(workerMenuButton(row));
      card.append(actions);
      grid.append(card);
      order.push(row.name);
    }
    container.append(grid);
  }
  const pools = new Map();
  for (const row of sorted.filter((item) => isPool(item) && !item.retired)) {
    const key = row.pool || row.kind;
    if (!pools.has(key)) pools.set(key, []);
    pools.get(key).push(row);
  }
  if (pools.size) {
    container.append(el("div", "group", icon("tent"), "Pools ", el("span", null, pools.size)));
    const grid = el("div", "pools");
    for (const [name, members] of pools) {
      const busy = members.filter((row) => row.pool_state === "busy").length;
      const strip = el("div", "pool-members");
      for (const row of members) {
        const lease = leaseInfo(row, now);
        const link = el("a", "pool-member", el("span", "sq" + (row.pool_state === "busy" ? " busy" : "")), el("span", "mono", row.name), el("span", "muted", lease.text));
        link.href = workerHash(row);
        link.dataset.worker = row.name;
        strip.append(link);
        order.push(row.name);
      }
      const profile = state.rows.profiles?.find((item) => item.name === members[0].profile);
      grid.append(el("article", "card",
        el("div", "card-head", workerTile(members[0]), el("div", "ident", el("strong", null, name), el("span", "sub", [members[0].topic, profile?.vcpus ? profile.vcpus + " vCPU each" : ""].filter(Boolean).join(" · "))),
          el("span", "muted", `${busy} busy · ${members.length - busy} idle`)),
        strip));
    }
    container.append(grid);
  }
  showMore(container, single.length);
  state.order = order;
  $("results").replaceChildren(container);
}

function renderJobs(visible) {
  const container = el("div", "jobs-list");
  const sorted = visible.slice().sort((a, b) => Number(filterTests.jobs.active(b)) - Number(filterTests.jobs.active(a)));
  for (const row of sorted.slice(0, state.limit)) {
    const [tone, ic] = jobTone(row.state);
    const link = el("a", "job-row", icon(ic, "tone-" + (tone || "info")), el("span", "name", row.job), el("span", "worker", row.worker), el("span", "pill" + (tone ? " " + tone : ""), row.state));
    link.href = workerHash({ name: row.worker }, row.job);
    link.dataset.worker = row.worker;
    link.dataset.job = row.job;
    container.append(link);
  }
  showMore(container, visible.length);
  $("results").replaceChildren(container);
}

function eventValue(value) {
  const key = value.toLowerCase();
  const tone = key === "exit 0" ? "good" : /^(exit |error$|failed$|failure$|shutoff$|deleted$)/.test(key) ? "bad" :
    /^(running|active|ready|succeeded|success|completed)$/.test(key) ? "good" : "busy";
  return el("strong", "tone-" + tone, value.toLowerCase());
}

function eventDetails(details) {
  if (!details) return [];
  if (details.includes(" -> ")) {
    const [before, after] = details.split(" -> ", 2);
    return [" ", eventValue(before), " → ", eventValue(after)];
  }
  const field = /^([a-z_-]+)=(.*)$/.exec(details);
  return field ? [" · ", eventValue(field[2])] : [" · ", eventValue(details)];
}

function renderEvents(visible, now) {
  const container = el("div");
  let day = "";
  const kinds = {
    "worker-added": ["add", "plus", "Created"], "worker-deleted": ["del", "minus", "Deleted"],
    "worker-state": ["chg", "swap", "Worker"], "job-added": ["job", "play", "Job started"], "job-state": ["job", "jobs", "Job"],
  };
  for (const row of visible.slice(0, state.limit)) {
    const label = dayLabel(row.timestamp);
    if (label !== day) {
      day = label;
      container.append(el("div", "day", label));
    }
    const [dot, ic, verb] = kinds[row.kind] || ["chg", "info", row.kind.replaceAll("-", " ")];
    const [worker, job] = String(row.target).split("/");
    const target = button(row.target, () => select(workerFor(worker), job), { cls: "target" });
    const meta = [row.profile, row.topic, row.agent, row.owner].filter(Boolean).join(" · ");
    container.append(el("div", "event",
      el("span", "event-dot " + dot, icon(ic)),
      el("div", null, el("div", "event-what", el("span", null, verb + " "), target, ...eventDetails(row.details)), meta ? el("div", "event-meta", meta) : null),
      el("div", "event-when", el("div", null, timeOfDay(row.timestamp)), el("div", null, relative(row.timestamp, now)))));
  }
  showMore(container, visible.length);
  $("results").replaceChildren(container);
}

function ramLabel(row) {
  return row.ram_gb != null ? row.ram_gb + " GB" : row.ram_mib == null ? "" : row.ram_mib % 1024 === 0 ? row.ram_mib / 1024 + " GiB" : row.ram_mib + " MiB";
}

function diskLabel(row) {
  return row.disk_total_bytes != null ? `${(row.disk_total_bytes / 2 ** 30).toFixed(0)} GiB disk` : row.disk ? row.disk + " GiB disk" : "";
}

function profileSpecs(row) {
  return [row.vcpus ? row.vcpus + " vCPU" : "", ramLabel(row), diskLabel(row)].filter(Boolean);
}

function renderProfiles(visible) {
  const grid = el("div", "profiles");
  for (const row of visible) {
    const head = el("div", "card-head", profileTile(row), el("div", "ident",
      el("span", null, el("span", "name", row.name), row.default ? el("span", "tag default", "default") : null),
      el("span", "sub", ciProviders[row.provider] || "OpenStack" + (row.flavor ? " · " + row.flavor : ""))));
    const specs = el("div", "specs", ...profileSpecs(row).map((spec) => el("span", "tag", spec)), row.arch ? el("span", "tag", row.arch) : null);
    const tags = (row.image_tags || []).join(", ");
    const actions = el("div", "card-actions");
    if (state.authenticated) actions.append(button("New worker", () => form("create", null, { profile: row.name }), { cls: "btn sm primary", ic: "plus" }));
    actions.append(button("Details", () => openProfile(row), { cls: "btn sm quiet", ic: "info" }));
    grid.append(el("article", "profile-card", head, specs, tags ? el("div", "sub", "Image tags: " + tags) : null, actions));
  }
  $("results").replaceChildren(grid);
}

async function refresh(manual = false) {
  if (!state.authenticated || loading) return;
  loading = true;
  const view = state.view;
  const access = state.token;
  try {
    const data = await request(view);
    if (state.token !== access) return;
    state.rows[view] = Array.isArray(data) ? data : [];
    if (view !== "workers" && !state.rows.workers) {
      state.rows.workers = await request("workers");
    }
    const stale = state.inventoryStale && view === "workers";
    connection(stale ? "Cached inventory" : "Live", stale ? "stale" : "online");
    if (state.selected && view === "workers") {
      const current = sourceWorker(state.selected.name);
      if (current) {
        const retired = Boolean(current.retired) !== Boolean(state.selected.retired);
        state.selected = current;
        if (retired) {
          state.follow = !current.retired;
          detailPanel();
          updateDetail(true);
        } else renderDetailHead();
      }
    }
    if (view === state.view) render();
  } catch (error) {
    if (state.authenticated) connection("Disconnected", "error");
    if (manual || !state.rows[view]) fail(error.message);
  } finally {
    loading = false;
    if (state.authenticated && state.view !== view) refresh();
  }
}

async function loadBackground() {
  try { state.rows.profiles = await request("profiles"); } catch { /* Shown when the view is opened. */ }
  try { state.rows.jobs = await request("jobs"); } catch { /* Shown when the view is opened. */ }
  lastRenderKey = "";
  render();
  if (state.selected) renderDetailHead();
}

async function openProfile(row) {
  const dialog = $("profile-dialog");
  const status = $("profile-status");
  const metadata = $("profile-metadata");
  $("profile-title").replaceChildren(el("span", "mono", row.name));
  $("profile-create").onclick = () => {
    dialog.close();
    form("create", null, { profile: row.name });
  };
  $("profile-create").hidden = !state.authenticated;
  const show = (profile) => {
    const ciProvider = ciProviders[profile.provider];
    const fields = [
      ["Provider", ciProvider || "OpenStack"],
      [ciProvider ? "Runner" : "Flavor", profile.flavor], ["Architecture", profile.arch],
      ["vCPUs", profile.vcpus], ["RAM", profile.ram_gb != null ? profile.ram_gb + " GB" :
        profile.ram_mib != null ? (profile.ram_mib * 2 ** 20 / 1e9).toFixed(1) + " GB" : null],
      ["Disk", profile.disk_total_bytes != null ? (profile.disk_total_bytes / 1e9).toFixed(1) + " GB" :
        profile.disk != null ? (profile.disk * 2 ** 30 / 1e9).toFixed(1) + " GB" : null],
      ["Tags", profile.image_tags?.join(", ")],
      ["Image name", profile.image_name || profile.image_status],
      ["Image ID", profile.image_id], ["Image created", profile.image_created_at],
    ];
    metadata.replaceChildren(...fields.filter(([, value]) => value != null && value !== "").map(([key, value]) =>
      el("div", "kv", el("dt", null, key), el("dd", null, el("span", null, value)))));
  };
  show(row);
  status.textContent = "Loading image details…";
  status.hidden = Object.hasOwn(ciProviders, row.provider);
  dialog.showModal();
  if (status.hidden) return;
  const create = $("profile-create").onclick;
  try {
    const [profile] = await request("profiles/" + encodeURIComponent(row.name));
    if (!dialog.open || $("profile-create").onclick !== create) return;
    if (profile) show(profile);
    status.hidden = true;
  } catch (error) {
    if (dialog.open && $("profile-create").onclick === create) status.textContent = error.message;
  }
}

// Worker detail
function workerHash(row, job) {
  return "#workers/" + encodeURIComponent(row.name) + (job ? "/" + encodeURIComponent(job) : "");
}

function select(row, job) {
  location.hash = workerHash(row, job);
}

function sourceWorker(name) {
  return (state.rows.workers || []).find((worker) => worker.name === name);
}

function workerFor(name) {
  return sourceWorker(name) || { name };
}

function splitActive() {
  return prefs.layout !== "cards" && state.view === "workers" && !narrow.matches;
}

function openWorker(row, job) {
  const same = state.selected?.name === row.name;
  state.selected = row;
  state.job = job;
  if (!same) {
    state.tab = "logs";
    state.detailJobs = null;
    state.logQuery = "";
    state.raw = false;
    loadDetailJobs(row);
  }
  state.follow = !row.retired;
  detailPanel();
  updateDetail(true);
  if (!splitActive()) {
    $("detail-output").scrollTop = 0;
    $("detail-head").querySelector("h2")?.focus({ preventScroll: true });
  }
}

async function loadDetailJobs(row) {
  try {
    const jobs = await request("jobs/" + encodeURIComponent(row.name));
    if (state.selected?.name !== row.name) return;
    state.detailJobs = Array.isArray(jobs) ? jobs : [];
    renderTabs();
    if (state.tab === "logs") renderLogTools();
  } catch { /* The jobs tab reports errors itself. */ }
}

function workerMenu(row) {
  const items = [];
  if (!isPool(row)) items.push({ label: "Extend lease", icon: "clock", hint: "E", run: () => form("extend", row) });
  items.push(
    { label: "Edit topic", icon: "pencil", hint: "T", run: () => { if (state.selected?.name === row.name) editTopic(); else form("topic", row); } },
    { label: "Claim worker", icon: "user", run: () => form("claim", row) },
    { label: "Register log file", icon: "file", run: () => form("register", row) },
    { label: "Renew cache token", icon: "key", run: () => form("renew-token", row) },
    "-",
    { label: "Copy SSH command", icon: "copy", run: () => copy("rworker ssh " + row.name) },
    { label: "Copy builder entry", icon: "link", run: async () => {
      try { await copy(await request("builder/" + encodeURIComponent(row.name))); } catch (error) { fail(error.message); }
    } },
  );
  if (row.run_url) items.push({ label: "Open GitHub run", icon: "external", run: () => window.open(row.run_url, "_blank", "noopener") });
  items.push("-", { label: "Delete worker…", icon: "trash", hint: "D", danger: true, run: () => form("delete", row) });
  return items;
}

function layoutWorkspace() {
  const row = state.selected;
  const split = splitActive();
  const fullPage = Boolean(row) && !split;
  $("workspace").classList.toggle("split", split);
  $("keybar").hidden = !(split && prefs.layout === "console" && state.authenticated);
  $("detail").hidden = !row;
  $("detail-empty").hidden = !split || Boolean(row);
  $("inventory").hidden = fullPage;
  $("app").classList.toggle("detail-open", fullPage);
  $("back").hidden = !fullPage;
  $("filters").hidden = fullPage;
  $("search-box").hidden = fullPage;
  if (fullPage) $("banner").hidden = true;
  $("create").hidden = !state.authenticated;
  $("fab").hidden = !state.authenticated;
  $("heading").textContent = views[state.view];
  $("banner").dataset.key = "";
  document.title = (row?.name || views[state.view]) + " · rworker";
}

function detailPanel() {
  layoutWorkspace();
  if (!state.selected) {
    closeLogStream();
    detailRequest++;
    detailLoading = false;
    lastHeadKey = "";
    $("detail-output").replaceChildren();
    return;
  }
  lastHeadKey = "";
  renderDetailHead();
  renderTabs();
}

function renderDetailHead() {
  const row = state.selected;
  if (!row) return;
  const now = Date.now() / 1000;
  const lease = leaseInfo(row, now);
  const split = splitActive();
  const key = JSON.stringify([row, lease.text, Math.round(lease.pct), split, state.order, Boolean(state.rows.profiles)]);
  if (key === lastHeadKey || $("detail-head").contains(document.activeElement) && document.activeElement.classList.contains("topic-input")) return;
  lastHeadKey = key;

  const name = el("h2", null, row.name);
  name.tabIndex = -1;
  const titleLine = el("div", null, name, iconButton("copy", "Copy worker name", () => copy(row.name)), statusPill(row));
  const topic = el("button", "topic-button", el("span", null, row.topic || (row.retired ? "No topic" : "Add a topic")));
  topic.type = "button";
  if (!row.retired) {
    topic.append(icon("pencil"));
    topic.title = "Edit topic (T)";
    topic.addEventListener("click", editTopic);
  } else topic.disabled = true;
  const nameBlock = el("div", "detail-name", titleLine, topic);

  const actions = el("div", "detail-actions");
  if (!row.retired) {
    actions.append(button("SSH", () => openTerminal(row), { cls: "btn primary", ic: "terminal", title: "Open an SSH terminal (S)" }),
      button("Start job", () => form("start", row), { cls: "btn", ic: "play" }));
    if (!isPool(row)) actions.append(button("Extend", () => form("extend", row), { cls: "btn", ic: "clock", title: "Extend the lease (E)" }));
    actions.append(workerMenuButton(row, "btn icon"));
  }

  const title = el("div", "detail-title", workerTile(row), nameBlock, actions);
  const parts = [];
  if (!split) {
    const index = state.order.indexOf(row.name);
    const crumbs = el("div", "crumbs");
    if (index >= 0 && state.order.length > 1) {
      const step = (delta) => {
        const next = state.order[index + delta];
        if (next) select(workerFor(next));
      };
      const previous = iconButton("chev-l", "Previous worker (K)", () => step(-1), "btn sm icon");
      const next = iconButton("chev-r", "Next worker (J)", () => step(1), "btn sm icon");
      previous.disabled = index === 0;
      next.disabled = index === state.order.length - 1;
      crumbs.append(el("span", "stepper", `${index + 1} of ${state.order.length}`, previous, next));
    }
    if (crumbs.childElementCount) parts.push(crumbs);
  }
  parts.push(title);

  const facts = el("dl", "facts");
  const profile = state.rows.profiles?.find((item) => item.name === row.profile);
  const fact = (label, value, cls = "") => facts.append(el("div", "fact" + (cls ? " " + cls : ""), el("dt", null, label), el("dd", null, value)));
  if (row.profile) {
    const value = el("span", null, glyph(profile ? profileGlyph(profile) : openstackGlyph, profile ? providerKey(profile) : "openstack"), " ", row.profile);
    if (profile) value.append(el("span", "muted", " · " + profileSpecs(profile).slice(0, 2).join(" · ")));
    fact("Profile", value);
  }
  fact("Agent", agentLabel(row.agent, row.agent_id));
  if (row.owner) fact("Owner", el("span", null, row.owner));
  if (lease.kind === "lease") fact("Lease", el("span", null, el("span", "lease-text" + (lease.tone ? " " + lease.tone : ""), lease.long), leaseBar(lease, true)), "lease-fact");
  else if (lease.kind === "pool") fact("Pool", el("span", null, el("span", "lease-text", (row.pool || row.kind) + " · " + lease.long.toLowerCase()), leaseBar(lease, true)), "lease-fact");
  else if (row.retired) fact("Created", el("span", null, clock(row.created_at)));
  parts.push(facts);
  $("detail-head").replaceChildren(...parts);
}

function editTopic() {
  const row = state.selected;
  if (!row || row.retired) return;
  const trigger = $("detail-head").querySelector(".topic-button");
  if (!trigger) return;
  const input = el("input", "topic-input");
  input.type = "text";
  input.maxLength = 255;
  input.value = row.topic || "";
  input.setAttribute("aria-label", "Topic");
  input.placeholder = "What is this worker for?";
  let done = false;
  const finish = async (save) => {
    if (done) return;
    done = true;
    const value = input.value.trim();
    lastHeadKey = "";
    if (save && value !== (row.topic || "")) {
      row.topic = value;
      renderDetailHead();
      await runAction({ action: "topic", worker: row.name, topic: value }, "Topic saved.");
    } else renderDetailHead();
  };
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") { event.preventDefault(); finish(true); }
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); finish(false); }
  });
  input.addEventListener("blur", () => finish(false));
  trigger.replaceWith(input);
  input.focus();
  input.select();
}

function renderTabs() {
  const row = state.selected;
  if (!row) return;
  const tabs = $("detail-tabs");
  tabs.replaceChildren();
  const running = state.detailJobs?.filter((job) => filterTests.jobs.active(job)).length;
  for (const [key, title, ic] of [["logs", "Logs", "terminal"], ["jobs", "Jobs", "jobs"], ["processes", "Processes", "cpu"], ["details", "Details", "info"]]) {
    if (row.retired && key === "processes") continue;
    const tab = button(title, () => {
      state.tab = key;
      renderTabs();
      updateDetail(true);
    }, { cls: "tab", ic });
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-selected", String(state.tab === key));
    if (key === "jobs" && state.detailJobs?.length) tab.append(el("span", "n", running ? running + " running" : state.detailJobs.length));
    tabs.append(tab);
  }
}

function openTerminal(row) {
  const dialog = $("terminal-dialog");
  const screen = $("terminal-screen");
  const status = $("terminal-status");
  const terminal = new Terminal({
    fontFamily: '"JetBrains Mono", "IBM Plex Mono", ui-monospace, monospace',
    cursorBlink: true,
    scrollback: 2000,
    theme: { background: "#101419", foreground: "#e8ecf5", cursor: "#9dcfff" },
  });
  const fit = new FitAddon.FitAddon();
  terminal.loadAddon(fit);
  $("terminal-title").textContent = row.name;
  status.textContent = "Connecting…";
  dialog.showModal();
  terminal.open(screen);
  fit.fit();

  const url = new URL("/v1/ui/follow/ssh/" + encodeURIComponent(row.name), location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  let ready = false;
  const dimensions = () => ({ cols: Math.min(terminal.cols, 512), rows: Math.min(terminal.rows, 256) });
  socket.onopen = () => {
    socket.send(JSON.stringify({ token: state.token, size: dimensions() }));
  };
  let failed = false;
  socket.onmessage = ({data}) => {
    if (typeof data === "string") {
      status.textContent = data;
      ready = data === "SSH session opened.";
      failed = !ready;
      if (ready) socket.send(JSON.stringify(dimensions()));
    } else {
      terminal.write(new Uint8Array(data));
    }
  };
  socket.onerror = () => {
    failed = true;
    status.textContent = "Could not connect to the SSH terminal.";
  };
  socket.onclose = () => {
    if (!failed) status.textContent = ready ? "Disconnected" : "SSH connection closed.";
    ready = false;
    terminal.options.disableStdin = true;
  };

  const encode = new TextEncoder();
  function input(bytes) {
    if (!ready || socket.readyState !== WebSocket.OPEN) return;
    if (socket.bufferedAmount + bytes.length > 512 * 1024) {
      status.textContent = "SSH connection is too slow.";
      failed = true;
      socket.close();
      return;
    }
    for (let i = 0; i < bytes.length; i += 16384) socket.send(bytes.subarray(i, i + 16384));
  }
  let sticky = false;
  const keys = $("term-keys");
  const sendKeys = (sequence) => { input(encode.encode(sequence)); terminal.focus(); };
  const setSticky = (on) => { sticky = on; keys.querySelector("[data-ctrl]")?.setAttribute("aria-pressed", String(on)); };
  keys.replaceChildren();
  for (const [label, sequence, title] of [
    ["Esc", "\x1b", "Escape"], ["Tab", "\t", "Tab"], ["Ctrl", null, "Hold Ctrl for the next key"], "-",
    ["^C", "\x03", "Interrupt (Ctrl+C)"], ["^D", "\x04", "End of input (Ctrl+D)"], ["^Z", "\x1a", "Suspend (Ctrl+Z)"],
    ["^L", "\x0c", "Clear screen (Ctrl+L)"], ["^U", "\x15", "Delete to line start (Ctrl+U)"], ["^W", "\x17", "Delete word (Ctrl+W)"],
    ["^R", "\x12", "Search history (Ctrl+R)"], ["^A", "\x01", "Line start (Ctrl+A)"], ["^E", "\x05", "Line end (Ctrl+E)"], "-",
    ["←", "\x1b[D", "Left"], ["↑", "\x1b[A", "Up"], ["↓", "\x1b[B", "Down"], ["→", "\x1b[C", "Right"],
  ]) {
    if (label === "-") { keys.append(el("span", "sep")); continue; }
    const key = button(label, () => sequence ? sendKeys(sequence) : (setSticky(!sticky), terminal.focus()), { cls: "", title, aria: title });
    key.tabIndex = -1;
    key.addEventListener("mousedown", (event) => event.preventDefault());
    if (!sequence) { key.dataset.ctrl = ""; key.setAttribute("aria-pressed", "false"); }
    keys.append(key);
  }
  terminal.onData((data) => {
    if (sticky && data.length === 1) {
      const code = data.toUpperCase().charCodeAt(0);
      setSticky(false);
      if (code >= 64 && code <= 95) { input(Uint8Array.of(code - 64)); return; }
    }
    input(encode.encode(data));
  });
  terminal.onBinary(data => input(Uint8Array.from(data, char => char.charCodeAt(0))));
  terminal.onResize(() => {
    if (ready && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(dimensions()));
  });
  const capture = $("terminal-fullscreen");
  const showCapture = () => {
    const on = document.fullscreenElement === dialog;
    capture.setAttribute("aria-pressed", String(on));
    capture.classList.toggle("on", on);
    capture.querySelector("span").textContent = on ? "Capturing keys" : "Capture all keys";
    if (on) status.textContent = navigator.keyboard?.lock ? "All keys go to the terminal. Hold Esc to leave." : "Full screen. This browser keeps some shortcuts; use the key bar for those. Press Esc to leave.";
    else if (ready) status.textContent = "SSH session opened.";
  };
  capture.onclick = async () => {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else {
        await dialog.requestFullscreen();
        await navigator.keyboard?.lock?.();
      }
    } catch { /* Full screen or keyboard lock refused. */ }
    terminal.focus();
  };
  document.addEventListener("fullscreenchange", showCapture);
  showCapture();
  const resize = new ResizeObserver(() => fit.fit());
  resize.observe(screen);
  dialog.addEventListener("close", () => {
    document.removeEventListener("fullscreenchange", showCapture);
    navigator.keyboard?.unlock?.();
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    resize.disconnect();
    socket.onopen = socket.onmessage = socket.onerror = socket.onclose = null;
    socket.close();
    terminal.dispose();
  }, { once: true });
  terminal.focus();
}

function closeLogStream() {
  const socket = logSocket;
  logSocket = undefined;
  logRetryAt = 0;
  socket?.close();
}

// Log viewer
function liveIndicator(text, tone = "") {
  const node = $("detail-output").querySelector(".live");
  if (!node) return;
  node.textContent = text;
  node.className = "live" + (tone ? " " + tone : "");
}

function logElements() {
  const output = $("detail-output");
  return { pre: output.querySelector(".log-frame pre"), jump: output.querySelector(".jump") };
}

// ANSI colors in logs (job prefixes from jobs.sh, compiler output): SGR
// codes become spans, every other escape sequence is dropped.
const ansiPalette = 16;
function ansi256(n) {
  if (n < 16) return null;
  if (n >= 232) { const v = 8 + (n - 232) * 10; return `rgb(${v},${v},${v})`; }
  const i = n - 16;
  const level = (x) => (x ? 55 + x * 40 : 0);
  return `rgb(${level(Math.floor(i / 36))},${level(Math.floor(i / 6) % 6)},${level(i % 6)})`;
}

function applySgr(st, params) {
  const codes = params === "" ? [0] : params.split(";").map((value) => Number(value) || 0);
  for (let i = 0; i < codes.length; i++) {
    const code = codes[i];
    if (code === 0) { st.bold = st.dim = st.italic = st.underline = false; st.fg = null; st.color = null; }
    else if (code === 1) st.bold = true;
    else if (code === 2) st.dim = true;
    else if (code === 3) st.italic = true;
    else if (code === 4) st.underline = true;
    else if (code === 22) st.bold = st.dim = false;
    else if (code === 23) st.italic = false;
    else if (code === 24) st.underline = false;
    else if (code >= 30 && code <= 37) { st.fg = code - 30; st.color = null; }
    else if (code >= 90 && code <= 97) { st.fg = code - 90 + 8; st.color = null; }
    else if (code === 39) { st.fg = null; st.color = null; }
    else if (code === 38 || code === 48) {
      const mode = codes[i + 1];
      if (mode === 5) {
        if (code === 38) {
          const n = codes[i + 2];
          if (n < ansiPalette) { st.fg = n; st.color = null; } else { st.fg = null; st.color = ansi256(n); }
        }
        i += 2;
      } else if (mode === 2) {
        if (code === 38) { st.fg = null; st.color = `rgb(${codes[i + 2] || 0},${codes[i + 3] || 0},${codes[i + 4] || 0})`; }
        i += 4;
      }
    }
  }
}

function ansiInto(target, text, st) {
  const pattern = /\x1b\[([0-9;?]*)[ -/]*([@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[^[\]]/g;
  let last = 0;
  const fragment = document.createDocumentFragment();
  const flush = (chunk) => {
    if (!chunk) return;
    const cls = [st.bold ? "b" : "", st.dim ? "d" : "", st.italic ? "it" : "", st.underline ? "u" : "", st.fg != null ? "f" + st.fg : ""].filter(Boolean).join(" ");
    if (!cls && !st.color) { fragment.append(chunk); return; }
    const span = document.createElement("span");
    if (cls) span.className = cls;
    if (st.color) span.style.color = st.color;
    span.append(chunk);
    fragment.append(span);
  };
  for (const match of text.matchAll(pattern)) {
    flush(text.slice(last, match.index));
    last = match.index + match[0].length;
    if (match[2] === "m") applySgr(st, match[1]);
  }
  flush(text.slice(last));
  target.append(fragment);
}

function setLog(text, { keepEnd = true } = {}) {
  state.logText = text;
  const { pre, jump } = logElements();
  if (!pre) return;
  const atEnd = pre.scrollHeight - pre.clientHeight - pre.scrollTop < 12;
  const query = state.logQuery.toLowerCase();
  // Hold back an escape sequence cut off at the end of a chunk.
  const safe = text.replace(/\x1b(\[[0-9;?]*[ -/]*)?$/, "");
  if (!safe) {
    pre.textContent = query ? "No lines match “" + state.logQuery + "”." : "No output yet.";
    pre.dataset.mode = "";
  } else if (query) {
    const lines = safe.split("\n").filter((line) => stripAnsi(line).toLowerCase().includes(query));
    pre.replaceChildren();
    if (lines.length) ansiInto(pre, lines.join("\n"), {});
    else pre.textContent = "No lines match “" + state.logQuery + "”.";
    pre.dataset.mode = "filter";
  } else if (pre.dataset.mode === "full" && safe.startsWith(state.logRendered)) {
    ansiInto(pre, safe.slice(state.logRendered.length), state.logSgr);
    state.logRendered = safe;
  } else {
    pre.replaceChildren();
    state.logSgr = {};
    ansiInto(pre, safe, state.logSgr);
    state.logRendered = safe;
    pre.dataset.mode = "full";
  }
  if (keepEnd && (atEnd || !pre.dataset.seen) && state.follow) pre.scrollTop = pre.scrollHeight;
  pre.dataset.seen = "1";
  if (jump) jump.hidden = pre.scrollHeight - pre.clientHeight - pre.scrollTop < 12;
}

function renderLogTools() {
  const row = state.selected;
  const tools = $("detail-output").querySelector(".log-tools");
  if (!row || !tools) return;
  const select = tools.querySelector("select");
  if (select) {
    const jobs = state.detailJobs || [];
    const options = [["", "All jobs"], ...jobs.map((job) => [job.job, job.job + " · " + job.state])];
    if (state.job && !jobs.some((job) => job.job === state.job)) options.push([state.job, state.job]);
    select.replaceChildren(...options.map(([value, label]) => {
      const option = el("option", null, label);
      option.value = value;
      return option;
    }));
    select.value = state.job || "";
  }
}

function buildLogPane(row) {
  const output = $("detail-output");
  const tools = el("div", "log-tools");
  const jobSelect = el("select");
  jobSelect.setAttribute("aria-label", "Job");
  jobSelect.addEventListener("change", () => select(row, jobSelect.value || null));
  const live = el("span", "live", row.retired ? "Archived" : "Snapshot");
  const search = el("label", "search", icon("search"));
  const input = el("input");
  input.type = "search";
  input.placeholder = "Filter lines";
  input.value = state.logQuery;
  input.setAttribute("aria-label", "Filter log lines");
  input.addEventListener("input", () => { state.logQuery = input.value; setLog(state.logText, { keepEnd: false }); });
  search.append(input);
  const toggle = (label, ic, on, change, title) => {
    const node = iconButton(ic, title, () => {
      change();
      node.classList.toggle("on");
      node.setAttribute("aria-pressed", String(node.classList.contains("on")));
    }, "btn sm icon" + (on ? " on" : ""));
    node.setAttribute("aria-pressed", String(on));
    return node;
  };
  tools.append(jobSelect, live, el("span", "spacer"), search);
  if (!row.retired) {
    const follow = button(state.follow ? "Following" : "Paused", () => setFollow(!state.follow), { cls: "btn sm follow" + (state.follow ? " on" : ""), ic: state.follow ? "pause" : "play", title: "Follow new output (F)" });
    follow.setAttribute("aria-pressed", String(state.follow));
    tools.append(follow);
  }
  tools.append(
    toggle("Wrap", "wrap", state.wrap, () => { state.wrap = !state.wrap; output.querySelector(".log-frame")?.classList.toggle("wrap", state.wrap); }, "Wrap long lines (W)"),
    toggle("Redact", "shield", state.redact, () => { state.redact = !state.redact; updateDetail(true); }, "Redact secrets"),
    iconButton("copy", "Copy log", () => copy(stripAnsi(state.logText)), "btn sm icon"),
    iconButton("download", "Download log", () => download(stripAnsi(state.logText), `${row.name}${state.job ? "-" + state.job : ""}.log`), "btn sm icon"));
  const frame = el("div", "log-frame" + (state.wrap ? " wrap" : ""));
  const pre = el("pre", null, "Loading…");
  pre.tabIndex = 0;
  const jump = button("Jump to latest", () => { pre.scrollTop = pre.scrollHeight; if (!state.follow && !row.retired) setFollow(true); }, { cls: "btn sm jump", ic: "down" });
  jump.hidden = true;
  pre.addEventListener("scroll", () => { jump.hidden = pre.scrollHeight - pre.clientHeight - pre.scrollTop < 12; }, { passive: true });
  frame.append(pre, jump);
  output.replaceChildren(tools, frame);
  renderLogTools();
}

function setFollow(enabled) {
  state.follow = enabled;
  const button = $("detail-output").querySelector(".follow");
  if (button) {
    button.classList.toggle("on", enabled);
    button.setAttribute("aria-pressed", String(enabled));
    button.replaceChildren(icon(enabled ? "pause" : "play"), el("span", null, enabled ? "Following" : "Paused"));
  }
  if (enabled) updateDetail(true);
  else {
    closeLogStream();
    liveIndicator("Paused");
  }
}

function followLogs(path) {
  if (logSocket || Date.now() < logRetryAt) return;
  liveIndicator("Connecting…", "wait");
  const url = new URL("/v1/ui/follow/" + path, location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(url);
  logSocket = socket;
  socket.binaryType = "arraybuffer";
  const decoder = new TextDecoder();
  let buffer = "";
  socket.onopen = () => { if (state.token) socket.send(state.token); };
  socket.onmessage = ({data}) => {
    if (logSocket !== socket) return;
    if (typeof data === "string") {
      buffer = JSON.parse(data).snapshot;
      liveIndicator("Cached");
    } else {
      buffer = (buffer + decoder.decode(data, {stream: true})).slice(-262144);
      liveIndicator("Live", "on");
    }
    setLog(buffer);
  };
  socket.onclose = () => {
    if (logSocket !== socket) return;
    logSocket = undefined;
    logRetryAt = Date.now() + 5000;
    liveIndicator("Reconnecting…", "wait");
  };
}

function flattenDetails(data, prefix = "", out = []) {
  for (const [key, value] of Object.entries(data || {})) {
    if (key === "metadata" || value == null || value === "" || (Array.isArray(value) && !value.length)) continue;
    const name = prefix + key;
    if (typeof value === "object" && !Array.isArray(value)) flattenDetails(value, name + ".", out);
    else out.push([name, value]);
  }
  return out;
}

const detailOrder = ["name", "status", "profile", "flavor", "owner", "agent", "agent_id", "topic", "ip", "ips.netbird", "ips.tailscale", "created_at", "expires_at", "grace_until", "max_lifetime_end", "niks3_token_expires_at", "image_name", "image_id", "availability_zone", "host_key", "run_url", "job_url"];
const timeFields = new Set(["created_at", "expires_at", "grace_until", "max_lifetime_end", "niks3_token_expires_at", "last_busy", "last_active", "last_seen"]);
const detailLabels = { ip: "IP", "ips.netbird": "NetBird IP", "ips.tailscale": "Tailscale IP", agent_id: "Agent ID", run_url: "GitHub run", job_url: "GitHub job", niks3_token_expires_at: "Cache token expires", max_lifetime_end: "Maximum lifetime", availability_zone: "Zone", host_key: "Host key", image_id: "Image ID", image_name: "Image" };

function renderDetails(data, boot) {
  const output = $("detail-output");
  const pane = el("div", "pane-pad");
  const tools = el("div", "pane-tools");
  const rawToggle = button(state.raw ? "Show fields" : "Raw JSON", () => { state.raw = !state.raw; renderDetails(data, boot); }, { cls: "btn sm", ic: state.raw ? "info" : "file" });
  tools.append(rawToggle, iconButton("copy", "Copy JSON", () => copy(JSON.stringify(boot || data, null, 2)), "btn sm icon"));
  if (!state.selected?.retired && !boot) {
    tools.append(button("Load boot metadata", async () => {
      try { renderDetails(data, await request("boot/" + encodeURIComponent(state.selected.name))); } catch (error) { fail(error.message); }
    }, { cls: "btn sm", ic: "download" }));
  }
  pane.append(tools);
  if (state.raw || boot || typeof data !== "object") {
    pane.append(el("pre", "raw", typeof data === "string" && !boot ? data : JSON.stringify(boot || data, null, 2)));
  } else {
    const fields = flattenDetails(data);
    const rank = (key) => { const index = detailOrder.indexOf(key); return index < 0 ? 999 : index; };
    fields.sort((a, b) => rank(a[0]) - rank(b[0]));
    const grid = el("dl", "kv-grid");
    for (const [key, value] of fields) {
      if (timeFields.has(key) && !(Number(value) > 0 || typeof value === "string")) continue;
      const shown = timeFields.has(key) ? clock(value) : Array.isArray(value) ? value.join(", ") : String(value);
      const label = detailLabels[key] || key.replaceAll("_", " ").replace(/^./, (c) => c.toUpperCase());
      const dd = el("dd");
      if (/^https:\/\//.test(shown)) {
        const link = el("a", null, shown);
        link.href = shown;
        link.target = "_blank";
        link.rel = "noopener";
        dd.append(link);
      } else dd.append(el("span", null, shown));
      dd.append(iconButton("copy", "Copy " + label, () => copy(String(value))));
      grid.append(el("div", "kv", el("dt", null, label), dd));
    }
    pane.append(grid);
  }
  output.replaceChildren(pane);
}

async function updateDetail(reset = false) {
  const row = state.selected;
  if (!row || !state.authenticated || (detailLoading && !reset)) return;
  detailLoading = true;
  const tab = state.tab;
  const target = encodeURIComponent(row.name);
  const job = state.job;
  const id = ++detailRequest;
  const destination = $("detail-output");
  if (reset) {
    closeLogStream();
    if (tab === "logs") buildLogPane(row);
    else destination.replaceChildren(emptyState("loader", "Loading…"));
  }
  try {
    if (tab === "logs") {
      if (!destination.querySelector(".log-frame")) buildLogPane(row);
      const live = !row.retired && state.follow;
      const endpoint = row.retired ? (state.redact ? "archived-logs" : "raw-archive") : (state.redact ? "logs" : "raw-logs");
      const path = target + (job ? "/" + encodeURIComponent(job) : "");
      if (live && Date.now() >= logRetryAt) {
        if (reset) setLog("");
      } else {
        const data = await request(endpoint + "/" + path).catch((error) => {
          if (!live) throw error;
          return state.logText;
        });
        if (id !== detailRequest) return;
        setLog(String(data ?? ""));
        if (!live) liveIndicator(row.retired ? "Archived" : state.follow ? "Reconnecting…" : "Paused", state.follow && !row.retired ? "wait" : "");
      }
      if (live) followLogs((state.redact ? "logs" : "raw-logs") + "/" + path);
      return;
    }
    let data;
    if (row.retired && tab === "details") data = row;
    else data = await request((tab === "details" ? "inspect" : tab) + "/" + target);
    if (id !== detailRequest) return;
    if (tab === "jobs") {
      state.detailJobs = Array.isArray(data) ? data : [];
      renderTabs();
      if (!state.detailJobs.length) {
        destination.replaceChildren(emptyState("jobs", "No jobs on this worker yet.", row.retired ? null : button("Start job", () => form("start", row), { cls: "btn primary", ic: "play" })));
        return;
      }
      const list = el("div", "list");
      for (const entry of state.detailJobs) {
        const [tone, ic] = jobTone(entry.state);
        list.append(el("div", "item", icon(ic, "tone-" + (tone || "info")), el("span", "name", entry.job), el("span", "pill" + (tone ? " " + tone : ""), entry.state),
          button("Logs", () => { state.tab = "logs"; select(row, entry.job); }, { cls: "btn sm", ic: "terminal" })));
      }
      destination.replaceChildren(el("div", "pane-pad", list));
      return;
    }
    if (tab === "processes") {
      if (String(data).trim().split("\n").length <= 1) {
        destination.replaceChildren(emptyState("cpu", "No running job processes."));
        return;
      }
      destination.replaceChildren(el("div", "pane-pad", el("pre", "raw", stripAnsi(String(data)))));
      return;
    }
    renderDetails(data);
  } catch (error) {
    if (id !== detailRequest) return;
    if (tab === "processes" && state.authenticated) {
      destination.replaceChildren(emptyState("cpu", error.message, button("Copy CLI command", () => copy("rworker ps " + row.name), { cls: "btn", ic: "copy" })));
    } else if (tab === "logs") {
      setLog(error.message);
      liveIndicator("Unavailable");
    } else destination.replaceChildren(emptyState("alert", error.message));
  } finally {
    if (id === detailRequest) detailLoading = false;
  }
}

async function copy(value) {
  try {
    await navigator.clipboard.writeText(value);
    toast("Copied to the clipboard.", { timeout: 2200 });
  } catch {
    fail("Could not access the clipboard.");
  }
}

function download(text, filename) {
  const link = el("a");
  link.href = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
  link.download = filename.replace(/[^A-Za-z0-9._-]/g, "_");
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(link.href), 1000);
}

// Menus: a popover on desktop, a bottom sheet on phones (CSS).
let menuState;
function closeMenu() {
  if (!menuState) return;
  menuState.menu.remove();
  menuState.scrim.remove();
  menuState = undefined;
}

function openMenu(anchor, items, point) {
  closeMenu();
  const scrim = el("div", "menu-scrim");
  const menu = el("div", "menu");
  menu.setAttribute("role", "menu");
  for (const item of items) {
    if (item === "-") { menu.append(el("hr")); continue; }
    const entry = el("button", item.danger ? "danger-quiet" : "", icon(item.icon), el("span", null, item.label), item.hint ? el("small", null, item.hint) : null);
    entry.type = "button";
    entry.setAttribute("role", "menuitem");
    entry.addEventListener("click", () => { closeMenu(); anchor.focus({ preventScroll: true }); item.run(); });
    menu.append(entry);
  }
  scrim.addEventListener("click", closeMenu);
  menu.addEventListener("keydown", (event) => {
    const entries = [...menu.querySelectorAll("button")];
    const index = entries.indexOf(document.activeElement);
    if (event.key === "Escape") { event.preventDefault(); closeMenu(); anchor.focus(); }
    if (event.key === "ArrowDown") { event.preventDefault(); entries[(index + 1) % entries.length].focus(); }
    if (event.key === "ArrowUp") { event.preventDefault(); entries[(index - 1 + entries.length) % entries.length].focus(); }
    if (event.key === "Tab") closeMenu();
  });
  document.body.append(scrim, menu);
  const box = point ? { top: point.y, bottom: point.y, right: point.x + menu.offsetWidth } : anchor.getBoundingClientRect();
  const top = box.bottom + (point ? 2 : 6) + menu.offsetHeight > innerHeight - 8 ? box.top - menu.offsetHeight - (point ? 2 : 6) : box.bottom + (point ? 2 : 6);
  menu.style.top = Math.max(8, top) + "px";
  menu.style.left = Math.min(Math.max(8, box.right - menu.offsetWidth), innerWidth - menu.offsetWidth - 8) + "px";
  menuState = { menu, scrim };
  menu.querySelector("button")?.focus();
}

// Action forms
const formTitles = {
  create: ["New worker", "Create worker"], extend: ["Extend lease", "Extend"],
  topic: ["Edit topic", "Save topic"], claim: ["Claim worker", "Claim worker"],
  delete: ["Delete worker", "Delete worker"], start: ["Start job", "Start job"],
  register: ["Register log file", "Register log"], "renew-token": ["Renew cache token", "Renew token"],
};
const lifetimes = ["30m", "1h", "2h", "4h", "8h", "24h"];

function field(label, name, { value = "", kind = "input", required = true, help, placeholder, attrs = {} } = {}) {
  const wrap = el("div", "field");
  const input = el(kind);
  input.id = "field-" + name;
  input.name = name;
  input.value = value;
  if (placeholder) input.placeholder = placeholder;
  input.required = required;
  for (const [key, attr] of Object.entries(attrs)) input.setAttribute(key, attr);
  const caption = el("label", null, label);
  caption.htmlFor = input.id;
  wrap.append(caption, input);
  if (help) wrap.append(el("small", null, help));
  return wrap;
}

function ttlSeconds(value) {
  const match = /^(?:(\d+)h)?(?:(\d+)m)?$/.exec(value || "");
  if (!match || (!match[1] && !match[2])) return null;
  return (Number(match[1] || 0) * 60 + Number(match[2] || 0)) * 60;
}

function whenLabel(seconds) {
  const date = new Date(seconds * 1000);
  const day = dayLabel(seconds);
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (day === "Today" ? "today" : day === "Yesterday" ? "yesterday" : Math.round((seconds - Date.now() / 1000) / 3600) < 36 ? "tomorrow" : date.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" })) + " at " + time;
}

function expiryPreview(wrap, worker) {
  const preview = el("p", "expiry-preview");
  const update = () => {
    const checked = wrap.querySelector('input[name="ttl"]:checked')?.value;
    const value = checked === "custom" ? wrap.querySelector('[name="ttl_custom"]').value.trim() : checked;
    const seconds = ttlSeconds(value);
    const now = Date.now() / 1000;
    preview.classList.remove("capped");
    if (seconds == null) {
      preview.replaceChildren(icon("clock"), el("span", "muted", "Enter a lifetime such as 90m, 2h or 1h30m."));
      return;
    }
    let expiry = now + seconds;
    let note = "in " + duration(seconds);
    if (worker?.max_lifetime_end > 0 && expiry > worker.max_lifetime_end) {
      expiry = worker.max_lifetime_end;
      note = "capped at its maximum lifetime";
      preview.classList.add("capped");
    }
    const parts = [icon(preview.classList.contains("capped") ? "alert" : "clock"), el("span", null, "New expiry: ", el("strong", null, whenLabel(expiry)), " · " + note)];
    if (worker?.expires_at > 0) parts.push(el("span", "muted", " (now " + (worker.expires_at > now ? whenLabel(worker.expires_at) : "overdue") + ")"));
    preview.replaceChildren(...parts);
  };
  wrap.addEventListener("change", update);
  wrap.addEventListener("input", update);
  update();
  return preview;
}

function lifetimeField(initial = "2h", help, worker) {
  const wrap = el("fieldset", "field");
  wrap.append(el("legend", null, "Lifetime"));
  const segs = el("div", "segs");
  const custom = el("input");
  custom.name = "ttl_custom";
  custom.id = "field-ttl-custom";
  custom.placeholder = "e.g. 90m or 12h";
  custom.setAttribute("aria-label", "Custom lifetime");
  const preset = lifetimes.includes(initial);
  for (const value of [...lifetimes, "custom"]) {
    const input = el("input");
    input.type = "radio";
    input.name = "ttl";
    input.value = value;
    input.checked = preset ? value === initial : value === "custom";
    input.addEventListener("change", () => { custom.hidden = value !== "custom"; if (value === "custom") custom.focus(); });
    segs.append(el("label", "seg", input, el("span", null, value === "custom" ? "Custom" : value)));
  }
  custom.hidden = preset;
  if (!preset) custom.value = initial;
  wrap.append(segs, custom, expiryPreview(wrap, worker));
  if (help) wrap.append(el("small", null, help));
  return wrap;
}

function toggleField(title, name, detail, checked = false, ic) {
  const input = el("input");
  input.type = "checkbox";
  input.name = name;
  input.checked = checked;
  const label = el("label", "switch");
  if (ic) label.append(icon(ic));
  label.append(el("span", null, el("strong", null, title), detail ? el("small", null, detail) : null), input);
  return label;
}

async function form(action, worker, options = {}) {
  if (!state.authenticated) return;
  closeMenu();
  currentAction = { action, ...(worker ? { worker: worker.name } : {}) };
  const dialog = $("dialog");
  const [title, submit] = formTitles[action];
  dialog.classList.toggle("drawer", action === "create");
  $("dialog-title").textContent = title;
  $("dialog-note").textContent = worker ? worker.name : "";
  $("dialog-note").className = "dialog-note" + (worker ? " mono" : "");
  $("dialog-hint").textContent = "";
  $("confirm").replaceChildren(submit);
  $("confirm").className = "btn " + (action === "delete" ? "danger" : "primary");
  $("form-error").hidden = true;
  const fields = $("form-fields");
  fields.replaceChildren();
  if (action === "create") {
    let profiles = state.rows.profiles;
    if (!profiles) {
      try { profiles = await request("profiles"); state.rows.profiles = profiles; } catch { profiles = []; }
    }
    if (!state.authenticated) return;
    const live = (state.rows.workers || []).filter((row) => !row.retired && !isPool(row)).length;
    $("dialog-note").textContent = live ? `${live} ad-hoc worker${live === 1 ? "" : "s"} running` : "";
    const initial = options.profile || profiles.find((item) => item.default)?.name || profiles[0]?.name || "standard";
    const picker = el("fieldset", "field");
    picker.append(el("legend", null, "Profile"));
    const list = el("div", "picker");
    if (profiles.length) {
      for (const profile of profiles) {
        const input = el("input");
        input.type = "radio";
        input.name = "profile";
        input.value = profile.name;
        input.checked = profile.name === initial;
        const sub = [ciProviders[profile.provider], ...profileSpecs(profile)].filter(Boolean).join(" · ");
        list.append(el("label", "pick", input, profileTile(profile),
          el("span", null, el("strong", null, profile.name), profile.default ? el("span", "tag default", "default") : null, el("small", null, sub || profile.flavor || "")),
          profile.arch ? el("span", "tag", profile.arch) : el("span")));
      }
    } else list.append(field("Profile name", "profile", { value: initial }));
    picker.append(list);
    fields.append(picker, lifetimeField(options.ttl || "2h", "Workers still in use when their lease ends get up to an hour of grace."),
      field("Topic", "topic", { value: options.topic || "", required: false, placeholder: "What is this worker for?", attrs: { maxlength: "255" } }),
      toggleField("Open SSH when ready", "open_ssh", "The terminal opens once the host key is pinned.", true, "terminal"));
    const advanced = el("details", "advanced");
    const overrides = el("div", "grid2",
      field("Flavor override", "flavor", { required: false }),
      field("Disk override (GiB)", "disk", { required: false, attrs: { type: "number", min: "1", max: "4294967295", step: "1" } }),
      field("Agent ID", "agent_id", { required: false, attrs: { maxlength: "128", pattern: "[A-Za-z0-9._:][A-Za-z0-9._:\\-]*" } }));
    advanced.append(el("summary", null, "Advanced: flavor, disk, agent ID, scratch, cache token"), overrides,
      el("div", "list", toggleField("Scratch worker", "scratch", "Lives up to 7 days instead of 24 hours."),
        toggleField("Skip the niks3 push token", "no_niks3_token", null)));
    fields.append(advanced);
    const updateProvider = () => {
      const name = fields.querySelector('input[name="profile"]:checked')?.value || fields.querySelector('[name="profile"]')?.value;
      const profile = profiles.find((row) => row.name === name);
      const ci = Object.hasOwn(ciProviders, profile?.provider) || /^(gh-(aarch64|x86_64)|gl-x86_64(-lease)?)$/.test(name || "");
      advanced.hidden = ci;
      for (const node of advanced.querySelectorAll("input")) node.disabled = ci;
      fields.querySelector("#field-flavor").placeholder = profile?.flavor || "Profile default";
      fields.querySelector("#field-disk").placeholder = profile?.disk ? String(profile.disk) : "Profile default";
    };
    list.addEventListener("change", updateProvider);
    updateProvider();
    $("dialog-hint").textContent = "Ready in 1 to 4 minutes";
  }
  if (action === "extend") {
    fields.append(lifetimeField(options.ttl || "2h", "The expiry is set to now plus this duration.", sourceWorker(worker.name) || worker));
  }
  if (["topic", "claim"].includes(action)) {
    fields.append(field("Topic", "topic", { value: worker?.topic || "", required: action === "claim", placeholder: "What is this worker for?", attrs: { maxlength: "255" } }));
    if (action === "claim") fields.prepend(el("p", "confirm-text muted", "Take over this worker: it is recorded as yours, with this topic."));
  }
  if (action === "start") {
    fields.append(field("Job name", "job", { placeholder: "e.g. cargo-test" }),
      field("Command", "command", { kind: "textarea", placeholder: "cd repo && nix develop -c cargo test", help: "Runs on the worker from the home directory. Output appears under Logs." }));
  }
  if (action === "register") {
    fields.append(field("Job name", "job"), field("Absolute log path on the worker", "path", { placeholder: "/home/agent/build.log" }));
  }
  if (action === "delete") {
    const running = state.selected?.name === worker.name ? (state.detailJobs || []).filter((job) => filterTests.jobs.active(job)) : [];
    fields.append(el("p", "confirm-text", "Delete ", el("span", "mono", worker.name), " and release its compute? Everything on the worker is lost."));
    if (running.length) fields.append(el("div", "warn-box", icon("alert"), el("span", null, `${running.length} job${running.length === 1 ? " is" : "s are"} still running: ${running.map((job) => job.job).join(", ")}.`)));
  }
  if (action === "renew-token") fields.append(el("p", "confirm-text", "Issue a new niks3 cache token for ", el("span", "mono", worker.name), "?"));
  dialog.showModal();
  const first = fields.querySelector("input:not([type=radio]):not([type=checkbox]), textarea") ;
  if (action !== "create" && first) first.focus();
  else if (action === "delete") $("dismiss").focus();
}

function collectForm(target) {
  const data = Object.fromEntries(new FormData(target));
  const action = { ...currentAction };
  for (const [key, value] of Object.entries(data)) if (key !== "ttl_custom") action[key] = typeof value === "string" ? value.trim() : value;
  if (data.ttl === "custom") action.ttl = String(data.ttl_custom || "").trim();
  return action;
}

async function runAction(action, message) {
  try {
    await request("action", action);
    toast(message);
    lastHeadKey = "";
    await refresh(true);
    if (state.selected && !logSocket && action.action !== "delete") updateDetail();
    if (state.selected) loadDetailJobs(state.selected);
    return true;
  } catch (error) {
    fail(error.message);
    return false;
  }
}

async function createWorker(action) {
  const openSsh = action.open_ssh === "on";
  delete action.open_ssh;
  for (const key of ["flavor", "disk", "agent_id"]) if (!action[key]) delete action[key];
  if (action.disk) action.disk = Number(action.disk);
  for (const key of ["scratch", "no_niks3_token"]) action[key] = action[key] === "on";
  const started = Date.now();
  const label = `${action.profile} · ${action.ttl}${action.topic ? " · " + action.topic : ""}`;
  const progress = toast("Creating a worker…", { tone: "busy", detail: label });
  const timer = setInterval(() => {
    const seconds = Math.floor((Date.now() - started) / 1000);
    progress.update("Creating a worker…", { tone: "busy", detail: `${label} · ${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}` });
  }, 1000);
  try {
    const result = await request("action", action);
    clearInterval(timer);
    progress.close();
    const name = result?.worker;
    toast(name ? `${name} is ready.` : "Worker is ready.", {
      timeout: 12000,
      actions: name ? [{ label: "Open", run: () => select({ name }) }, { label: "SSH", run: () => openTerminal({ name }) }] : [],
    });
    if (openSsh && name) openTerminal({ name });
    await refresh(true);
  } catch (error) {
    clearInterval(timer);
    progress.close();
    toast("Could not create the worker.", { tone: "error", detail: error.message, actions: [{ label: "Try again", run: () => form("create", null, action) }] });
  }
}

const actionMessages = {
  extend: (a) => `${a.worker} now expires in ${a.ttl}.`,
  topic: () => "Topic saved.",
  claim: (a) => `You now own ${a.worker}.`,
  start: (a) => `Started ${a.job}.`,
  register: (a) => `Registered ${a.job}.`,
  "renew-token": () => "Cache token renewed.",
  delete: (a) => `Deleted ${a.worker}.`,
};

$("action-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const action = collectForm(event.target);
  const missing = [...event.target.querySelectorAll("[required]")].find((node) => !node.disabled && !node.closest("[hidden]") && !node.value.trim());
  const problem = missing ? `Fill in ${missing.labels?.[0]?.textContent || missing.name}.` :
    ["create", "extend"].includes(action.action) && !/^\d[\dhm]{0,15}$/.test(action.ttl || "") ? "Enter a lifetime such as 90m, 2h or 1h30m." : "";
  if (problem) {
    $("form-error").textContent = problem;
    $("form-error").hidden = false;
    (missing || $("field-ttl-custom"))?.focus();
    return;
  }
  if (action.action === "create") {
    $("dialog").close();
    createWorker(action);
    return;
  }
  $("confirm").disabled = true;
  $("confirm").prepend(icon("loader", "spin"));
  $("form-error").hidden = true;
  try {
    await request("action", action);
    $("dialog").close();
    toast(actionMessages[action.action](action));
    if (action.action === "delete") location.hash = "workers";
    lastHeadKey = "";
    await refresh(true);
    if (state.selected && action.action !== "delete") {
      loadDetailJobs(state.selected);
      if (action.action === "start") { state.tab = "logs"; select(state.selected, action.job); }
      else if (!logSocket) updateDetail();
    }
  } catch (error) {
    $("form-error").textContent = error.message;
    $("form-error").hidden = false;
  } finally {
    $("confirm").disabled = false;
    $("confirm").querySelector(".spin")?.remove();
  }
});
$("action-form").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
    event.preventDefault();
    $("action-form").requestSubmit();
  }
});

// Command palette
const palette = { items: [], active: 0 };

function paletteEntries(ttl) {
  const entries = [];
  const add = (section, ic, label, keywords, run, hint = "") => {
    const entry = { section, ic, label, keywords: keywords.toLowerCase(), run, hint };
    entries.push(entry);
    return entry;
  };
  const selected = state.selected;
  if (selected && !selected.retired) {
    const name = selected.name;
    const tag = (text) => el("span", null, text + " ", el("span", "mono", name));
    add("This worker", "terminal", tag("Open SSH on"), "ssh terminal shell " + name, () => openTerminal(selected), "S");
    if (!isPool(selected)) add("This worker", "clock", ttl ? el("span", null, "Extend ", el("span", "mono", name), " to " + ttl + " from now") : tag("Extend"), "extend lease ttl " + name, () => ttl ? runAction({ action: "extend", worker: name, ttl }, `${name} now expires in ${ttl}.`) : form("extend", selected), "E");
    add("This worker", "play", tag("Start a job on"), "start job run command " + name, () => form("start", selected));
    add("This worker", "pencil", tag("Edit topic of"), "topic rename edit " + name, () => editTopic(), "T");
    add("This worker", "copy", tag("Copy SSH command for"), "copy ssh command " + name, () => copy("rworker ssh " + name));
    add("This worker", "trash", tag("Delete"), "delete down remove destroy " + name, () => form("delete", selected), "D");
  }
  add("Actions", "plus", el("span", null, "New worker"), "new create worker up", () => form("create", null, ttl ? { ttl } : {}), "N");
  for (const profile of state.rows.profiles || []) {
    add("Actions", "plus", el("span", null, "New worker from ", el("span", "mono", profile.name)), "new create worker profile " + profile.name + " " + (profile.arch || ""), () => form("create", null, { profile: profile.name, ...(ttl ? { ttl } : {}) }));
  }
  for (const row of (state.rows.workers || []).filter((item) => !item.retired)) {
    const now = Date.now() / 1000;
    const label = el("span", null, el("span", "mono", row.name), el("small", null, " · " + [workerSub(row), leaseInfo(row, now).text].filter(Boolean).join(" · ")));
    add("Workers", null, label, ["open", row.name, row.topic, row.agent, row.owner, row.profile].filter(Boolean).join(" "), () => select(row), "open").glyphRow = row;
    if (!isPool(row) && row.name !== selected?.name) {
      add("Workers", "clock", ttl ? el("span", null, "Extend ", el("span", "mono", row.name), " to " + ttl + " from now") : el("span", null, "Extend ", el("span", "mono", row.name)),
        "extend lease ttl " + row.name + " " + (row.topic || ""), () => ttl ? runAction({ action: "extend", worker: row.name, ttl }, `${row.name} now expires in ${ttl}.`) : form("extend", row));
      add("Workers", "terminal", el("span", null, "SSH into ", el("span", "mono", row.name)), "ssh terminal " + row.name + " " + (row.topic || ""), () => openTerminal(row));
    }
  }
  for (const [view, label] of Object.entries(views)) add("Go to", { workers: "server", jobs: "jobs", events: "activity", profiles: "layers" }[view], el("span", null, label), "go view " + view, () => { location.hash = view; });
  for (const [layout, label, ic] of [["split", "split view", "columns"], ["cards", "cards", "grid"], ["console", "console", "terminal"]]) {
    if (layout !== prefs.layout) add("Settings", ic, el("span", null, "Switch to the " + label + " layout"), "layout switch view design " + layout + " " + label, () => setPref("layout", layout));
  }
  add("Settings", "sun", el("span", null, "Theme: system, dark or light"), "theme dark light mode appearance", () => openSettings());
  add("Settings", "keyboard", el("span", null, "Keyboard shortcuts"), "keyboard shortcuts help keys", () => showKeys(), "?");
  if (state.token) add("Settings", "lock", el("span", null, "Lock console"), "lock sign out logout", () => lock());
  return entries;
}

function itemNode(item, index) {
  const node = el("button", "palette-item");
  node.type = "button";
  node.id = "palette-item-" + index;
  node.setAttribute("role", "option");
  node.setAttribute("aria-selected", String(index === palette.active));
  if (item.glyphRow) node.append(glyph(workerGlyph(item.glyphRow)));
  else node.append(icon(item.ic || "info"));
  node.append(el("span", "text", item.label), item.hint ? el("span", "hint", item.hint.length === 1 ? el("kbd", null, item.hint) : item.hint) : el("span"));
  node.addEventListener("click", () => runPalette(index));
  node.addEventListener("mousemove", () => { if (palette.active !== index) { palette.active = index; highlightPalette(); } });
  return node;
}

function renderPalette() {
  const query = $("palette-input").value.trim().toLowerCase();
  const words = query.split(/\s+/).filter(Boolean);
  const ttl = words.find((word) => /^\d+[hm](\d+m)?$/.test(word));
  const terms = words.filter((word) => word !== ttl);
  let items = paletteEntries(ttl).filter((item) => terms.every((term) => item.keywords.includes(term)));
  if (!terms.length && !ttl) items = items.filter((item) => item.section !== "Workers" || item.glyphRow);
  palette.items = items.slice(0, 60);
  palette.active = Math.min(palette.active, Math.max(0, palette.items.length - 1));
  const list = $("palette-list");
  list.replaceChildren();
  if (!palette.items.length) {
    list.append(el("div", "palette-empty", "Nothing matches. Try a worker name, “ssh”, “extend 1h” or “new”."));
    return;
  }
  let section = "";
  palette.items.forEach((item, index) => {
    if (item.section !== section) {
      section = item.section;
      list.append(el("div", "palette-section", section));
    }
    list.append(itemNode(item, index));
  });
  highlightPalette();
}

function highlightPalette() {
  $("palette-list").querySelectorAll(".palette-item").forEach((node, index) => node.setAttribute("aria-selected", String(index === palette.active)));
  const active = $("palette-item-" + palette.active);
  active?.scrollIntoView({ block: "nearest" });
  $("palette-input").setAttribute("aria-activedescendant", active ? active.id : "");
}

function runPalette(index) {
  const item = palette.items[index];
  if (!item) return;
  $("palette").close();
  item.run();
}

function openPalette() {
  if (!state.authenticated) return;
  closeMenu();
  for (const dialog of document.querySelectorAll("dialog[open]")) if (dialog.id !== "terminal-dialog") dialog.close();
  palette.active = 0;
  $("palette-input").value = "";
  renderPalette();
  $("palette").showModal();
  $("palette-input").focus();
}

$("palette-input").addEventListener("input", () => { palette.active = 0; renderPalette(); });
$("palette-input").addEventListener("keydown", (event) => {
  if (event.key === "ArrowDown") { event.preventDefault(); palette.active = Math.min(palette.items.length - 1, palette.active + 1); highlightPalette(); }
  if (event.key === "ArrowUp") { event.preventDefault(); palette.active = Math.max(0, palette.active - 1); highlightPalette(); }
  if (event.key === "Enter") { event.preventDefault(); runPalette(palette.active); }
});
$("palette").addEventListener("click", (event) => { if (event.target === $("palette")) $("palette").close(); });

// Settings and shortcut help
function openSettings() {
  for (const [key, value] of Object.entries(prefs)) {
    for (const input of document.querySelectorAll(`#settings input[name="${key}"]`)) input.checked = input.value === value;
  }
  $("settings").showModal();
}
document.querySelectorAll("#settings input[type=radio]").forEach((input) => input.addEventListener("change", () => setPref(input.name, input.value)));

const shortcuts = [
  [["Ctrl", "K"], "Jump to a worker or run an action"], [["/"], "Filter the current view"],
  [["1"], "Workers"], [["2"], "Jobs"], [["3"], "Events"], [["4"], "Profiles"],
  [["N"], "New worker"], [["J"], "Next worker"], [["K"], "Previous worker"], [["Enter"], "Open the focused worker"],
  [["Esc"], "Close the worker or dialog"], [["S"], "SSH into the open worker"], [["E"], "Extend its lease"],
  [["T"], "Edit its topic"], [["D"], "Delete it"], [["L"], "Show its logs"], [["F"], "Follow or pause the log"],
  [["W"], "Wrap long log lines"], [["R"], "Refresh"], [["?"], "This list"],
];
function showKeys() {
  $("keys-list").replaceChildren(...shortcuts.flatMap(([keys, text]) => [el("dt", null, ...keys.map((key) => el("kbd", null, key))), el("dd", null, text)]));
  $("keys").showModal();
}

function moveSelection(delta) {
  if (state.view !== "workers") return;
  if (state.selected) {
    const index = state.order.indexOf(state.selected.name);
    const next = state.order[index < 0 ? 0 : index + delta];
    if (next) select(workerFor(next));
    return;
  }
  const items = [...document.querySelectorAll("#results .worker-item, #results .card .name, #results .pool-member")];
  if (!items.length) return;
  const index = items.indexOf(document.activeElement);
  (items[index < 0 ? (delta > 0 ? 0 : items.length - 1) : Math.min(items.length - 1, Math.max(0, index + delta))]).focus();
}

document.addEventListener("keydown", (event) => {
  if (!state.authenticated) return;
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    if ($("palette").open) $("palette").close();
    else openPalette();
    return;
  }
  if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
  if (document.querySelector("dialog[open]") || menuState) return;
  const target = event.target;
  if (target.closest?.("input, textarea, select, [contenteditable=true], .xterm")) {
    if (event.key === "Escape" && target.id === "search") {
      target.value = "";
      target.blur();
      render();
    }
    return;
  }
  const row = state.selected;
  const live = row && !row.retired;
  const keys = {
    "/": () => $("search-box").hidden || $("search").focus(),
    "?": showKeys,
    "1": () => { location.hash = "workers"; }, "2": () => { location.hash = "jobs"; },
    "3": () => { location.hash = "events"; }, "4": () => { location.hash = "profiles"; },
    n: () => form("create"), j: () => moveSelection(1), k: () => moveSelection(-1),
    r: () => refresh(true),
    Escape: () => { if (row) location.hash = "workers"; },
    s: () => live && openTerminal(row),
    e: () => live && !isPool(row) && form("extend", row),
    t: () => live && editTopic(),
    d: () => live && form("delete", row),
    l: () => { if (row) { state.tab = "logs"; renderTabs(); updateDetail(true); } },
    f: () => live && state.tab === "logs" && setFollow(!state.follow),
    w: () => row && state.tab === "logs" && $("detail-output").querySelector('[aria-label^="Wrap"]')?.click(),
  };
  const handler = keys[event.key];
  if (!handler) return;
  event.preventDefault();
  handler();
});

// Right-click (or long-press) on a worker or job opens its actions.
function contextItems(target) {
  const row = workerFor(target.dataset.worker);
  const job = target.dataset.job;
  const items = [];
  if (job) {
    items.push({ label: "Open job log", icon: "terminal", run: () => { state.tab = "logs"; select(row, job); } },
      { label: "Copy job name", icon: "copy", run: () => copy(job) }, "-");
  }
  items.push({ label: "Open worker", icon: "server", hint: "Enter", run: () => select(row) });
  if (row.retired) {
    items.push({ label: "Copy worker name", icon: "copy", run: () => copy(row.name) });
    return items;
  }
  items.push({ label: "Open SSH", icon: "terminal", hint: "S", run: () => openTerminal(row) },
    { label: "Start job", icon: "play", run: () => form("start", row) },
    { label: "Copy worker name", icon: "copy", run: () => copy(row.name) }, "-");
  return items.concat(workerMenu(row).map((item) => item !== "-" && item.label === "Edit topic" ? { ...item, run: () => form("topic", row) } : item));
}

document.addEventListener("contextmenu", (event) => {
  if (!state.authenticated || event.shiftKey) return;
  const target = event.target.closest?.("#results [data-worker]");
  if (!target || !target.dataset.worker) return;
  event.preventDefault();
  openMenu(target, contextItems(target), { x: event.clientX, y: event.clientY });
});

function followInventory() {
  if (!state.authenticated || stream) return;
  const url = new URL("/v1/ui/updates", location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(url);
  stream = socket;
  socket.onmessage = ({data}) => {
    if (data === "updated" && state.view === "workers") refresh();
  };
  socket.onclose = () => {
    if (stream !== socket) return;
    stream = undefined;
    setTimeout(followInventory, 5000);
  };
}

async function openConsole(token = "", automatic = false) {
  state.token = token;
  try {
    const session = await request("session");
    state.authenticated = true;
    if (token) {
      try { sessionStorage.setItem("rworker.operator", token); } catch { /* Storage disabled. */ }
    }
    $("login").hidden = true;
    $("login-error").hidden = true;
    $("console").hidden = false;
    $("toolbar").hidden = false;
    $("lock").hidden = !token;
    $("session-detail").textContent = token ? "Operator token" : "Signed in";
    if (!token && session?.signOutUrl?.startsWith("https://")) {
      $("sign-out").href = session.signOutUrl;
      $("sign-out").hidden = false;
    }
    navigate();
    followInventory();
    loadBackground();
  } catch (error) {
    lock();
    if (!automatic || error.message !== "Enter the console access token.") {
      $("login-error").textContent = error.message;
      $("login-error").hidden = false;
    }
  }
}

$("unlock").addEventListener("submit", (event) => {
  event.preventDefault();
  const token = $("token").value.trim();
  $("token").value = "";
  if (token) openConsole(token);
});
$("lock").addEventListener("click", () => lock());
$("create").addEventListener("click", () => form("create"));
$("fab").addEventListener("click", () => form("create"));
$("back").addEventListener("click", () => { location.hash = state.view; });
$("palette-open").addEventListener("click", openPalette);
$("settings-open").addEventListener("click", openSettings);
$("settings-mobile").addEventListener("click", openSettings);
$("settings-close").addEventListener("click", () => $("settings").close());
$("keys-close").addEventListener("click", () => $("keys").close());
$("terminal-close").addEventListener("click", () => $("terminal-dialog").close());
$("profile-close").addEventListener("click", () => $("profile-dialog").close());
$("cancel").addEventListener("click", () => $("dialog").close());
$("dismiss").addEventListener("click", () => $("dialog").close());
$("search").addEventListener("input", () => { state.limit = 200; render(); });
for (const dialog of document.querySelectorAll("dialog:not(#terminal-dialog)")) {
  dialog.addEventListener("click", (event) => { if (event.target === dialog) dialog.close(); });
}
narrow.addEventListener("change", () => { detailPanel(); lastRenderKey = ""; render(); });
window.addEventListener("hashchange", navigate);
setInterval(() => {
  if (!state.authenticated || document.hidden) return;
  if (state.view !== "profiles") refresh();
  else render();
}, 5000);
setInterval(() => { if (state.authenticated && !document.hidden && state.selected && state.tab === "logs" && state.follow && !state.selected.retired && !logSocket) updateDetail(); }, 2500);
$("toolbar").hidden = true;
navigate();
let savedToken = "";
try { savedToken = sessionStorage.getItem("rworker.operator") || ""; } catch { /* Storage disabled. */ }
openConsole(savedToken, true);
