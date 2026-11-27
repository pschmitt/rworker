"use strict";

const $ = (id) => document.getElementById(id);
const root = document.documentElement;
const SVG_NS = "http://www.w3.org/2000/svg";
const views = { workers: "Workers", jobs: "Jobs", logs: "Logs", profiles: "Profiles", events: "Events" };
const narrow = matchMedia("(max-width: 1180px)");
const leaseWarningSeconds = 15 * 60;

// Preferences: layout (split view or cards), theme and density, kept per browser.
const prefChoices = { layout: ["split", "cards", "console"], theme: ["system", "dark", "light"], density: ["comfortable", "compact"], rail: ["expanded", "collapsed"] };
const prefs = { layout: "split", theme: "system", density: "comfortable", rail: "expanded" };
try { Object.assign(prefs, JSON.parse(localStorage.getItem("rworker.prefs") || "{}")); } catch { /* Storage disabled. */ }
for (const [key, choices] of Object.entries(prefChoices)) if (!choices.includes(prefs[key])) prefs[key] = choices[0];

function applyPrefs() {
  root.dataset.theme = prefs.theme;
  root.dataset.layout = prefs.layout;
  root.dataset.density = prefs.density;
  root.dataset.rail = prefs.rail;
  const toggle = document.getElementById("rail-toggle");
  if (toggle) {
    const collapsed = prefs.rail === "collapsed";
    toggle.title = (collapsed ? "Expand" : "Collapse") + " the sidebar ([)";
    toggle.setAttribute("aria-expanded", String(!collapsed));
    toggle.querySelector(".lbl").textContent = collapsed ? "Expand" : "Collapse";
  }
}

function toggleRail() {
  setPref("rail", prefs.rail === "collapsed" ? "expanded" : "collapsed");
  applySplit();
}

function setPref(key, value) {
  if (!prefChoices[key]?.includes(value)) return;
  prefs[key] = value;
  try { localStorage.setItem("rworker.prefs", JSON.stringify(prefs)); } catch { /* Storage disabled. */ }
  applyPrefs();
  for (const input of document.querySelectorAll(`#settings input[name="${key}"]`)) input.checked = input.value === value;
  $("splitter").setAttribute("aria-orientation", prefs.layout === "console" ? "horizontal" : "vertical");
  lastRenderKey = "";
  detailPanel();
  render();
}
applyPrefs();

const state = {
  token: "", authenticated: false, view: "workers", rows: {}, selected: null, job: null, tab: "logs",
  filters: { workers: "live", jobs: "active", events: "all" }, sort: "newest", tableSort: null,
  redact: true, follow: true, wrap: true, logQuery: "", logText: "", logRendered: "", logSgr: {}, limit: 200,
  detailJobs: null, raw: false, dismissed: new Set(), order: [],
  logsWorker: "", logsJob: "", logsJobs: null, logsKey: "",
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
  // The installed app's "New worker" shortcut opens #new.
  if (location.hash === "#new") {
    history.replaceState(null, "", "#workers");
    if (state.authenticated) setTimeout(() => form("create"));
    else state.pendingCreate = true;
  }
  const [requested, worker, job] = location.hash.slice(1).split("/");
  const view = Object.hasOwn(views, requested) ? requested : "workers";
  if (view !== state.view) {
    if (state.logFullscreen) setLogFullscreen(false);
    state.limit = 200;
    $("search").value = "";
    state.logQuery = "";
    syncSearchOpen();
  }
  state.view = view;
  document.querySelectorAll("[data-view]").forEach((link) => {
    if (link.dataset.view === state.view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  $("search").placeholder = { workers: "Filter workers", jobs: "Filter jobs", logs: "Filter lines", events: "Filter events", profiles: "Filter profiles" }[view];
  if (view === "logs") {
    state.logsWorker = worker ? decodeURIComponent(worker) : "";
    state.logsJob = worker && job ? decodeURIComponent(job) : "";
  }
  if (state.authenticated && detailViews.has(view) && worker) {
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
  // A registered log file: the job is live, its process is not ours to track.
  if (key === "registered") return ["busy", "activity"];
  if (key === "exit 0") return ["good", "check"];
  if (/^exit /.test(key) || /error|failed/.test(key)) return ["bad", "x"];
  if (key === "archived") return ["archived", "archive"];
  return ["", "clock"];
}

// A job state pill: running and registered keep the live dot, finished and
// archived jobs show what happened instead.
function jobPill(value) {
  const [tone, ic] = jobTone(value);
  const live = ["running", "registered"].includes(String(value).toLowerCase());
  const pill = el("span", "pill" + (tone ? " " + tone : "") + (live ? "" : " plain"));
  if (!live) pill.append(icon(ic));
  pill.append(String(value));
  return pill;
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
  logs: [],
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
    const sort = dropdown({ label: "Sort workers", onChange: (value) => { state.sort = value; state.tableSort = null; render(); } });
    sort.setOptions([
      { value: "newest", label: "Newest first", sub: "Most recently created on top", icon: "sort" },
      { value: "lease", label: "Expiring first", sub: "Shortest lease on top", icon: "clock" },
      { value: "name", label: "Name", sub: "Alphabetical", icon: "sort" },
    ]);
    sort.setValue(state.sort);
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
  $("inventory").classList.toggle("fill", view === "logs");
  if (view === "logs") {
    renderLogsView();
    return;
  }
  const source = state.rows[view];
  const results = $("results");
  const search = $("search").value.trim().toLowerCase();
  if (!source) {
    if (lastRenderKey !== "loading:" + view) results.replaceChildren(emptyState("loader", "Loading " + view + "…"));
    lastRenderKey = "loading:" + view;
    return;
  }
  const test = filterTests[view]?.[state.filters[view]] || (() => true);
  // Jobs also match the topic and agent shown in the list.
  const workerOf = view === "jobs" && search ? new Map((state.rows.workers || []).map((worker) => [worker.name, worker])) : null;
  const searchable = (row) => workerOf ? { ...row, topic: row.topic || workerOf.get(row.worker)?.topic, agent: workerOf.get(row.worker)?.agent } : row;
  let visible = source.filter((row) => searchMatch(searchable(row), search) && test(row, now));
  if (view === "events") visible = visible.slice().reverse();
  const key = JSON.stringify([view, prefs.layout, state.filters[view], state.sort, state.tableSort, state.jobSort, state.profileSort, search, state.limit,
    state.selected?.name, state.job, view === "jobs" ? (state.rows.workers || []).length : 0, Math.floor(now / 20), visible]);
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
    case "jobs":
      if (prefs.layout === "cards") renderJobCards(visible);
      else if (prefs.layout === "console" && !narrow.matches) renderJobTable(visible);
      else renderJobs(visible);
      break;
    case "events": renderEvents(visible, now); break;
    default:
      if (prefs.layout === "console" && !narrow.matches) renderProfileTable(visible);
      else if (prefs.layout === "split") renderProfileList(visible);
      else renderProfiles(visible);
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

// Workers created in the last five minutes get a "new" badge.
function isNew(row, now) {
  const created = timestamp(row.created_at);
  return !row.retired && Number.isFinite(created) && now - created < 300;
}

function newBadge(row, now) {
  return isNew(row, now) ? el("span", "tag new", "new") : null;
}

function workerSub(row) {
  return row.topic || row.label || (isPool(row) ? row.kind + " pool" : row.owner) || "";
}

function workerRow(row, now) {
  const lease = leaseInfo(row, now);
  const data = JSON.stringify([row.topic, row.label, row.status, row.retired, lease.text, lease.tone, Math.round(lease.pct / 5), row.icon, isNew(row, now)]);
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
  node.append(workerTile(row), el("span", "ident", el("span", "name-line", el("span", "name", row.name), newBadge(row, now)), el("span", "sub", workerSub(row))), right);
  // Flash only on changes worth noticing, not on the lease countdown.
  const notable = JSON.stringify([row.topic, row.label, row.status, row.retired, row.pool_state, row.agent, lease.tone]);
  if (cached && cached.notable !== notable) node.classList.add("flash");
  rowCache.set(row.name, { node, data, notable });
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
  const data = JSON.stringify([row.topic, row.agent, row.status, row.retired, lease.text, lease.tone, Math.round(lease.pct / 5), isNew(row, now)]);
  const current = state.selected?.name === row.name;
  const cached = tableCache.get(row.name);
  if (cached && cached.data === data) {
    if (current) cached.node.setAttribute("aria-current", "true");
    else cached.node.removeAttribute("aria-current");
    return cached.node;
  }
  const status = row.retired ? ["retired", "idle"] : lease.kind === "pool" ? [row.pool_state, lease.tone === "busy" ? "busy" : "idle"] :
    [String(row.status || "?").toLowerCase(), /^(active|ready)$/i.test(row.status) ? "good" : /^(build|boot|deleting|pending)/i.test(row.status) ? "busy" : "bad"];
  const kind = workerKind(row);
  const glyphChar = workerGlyph(row);
  const glyphClass = glyphChar === githubGlyph ? "github" : glyphChar === gitlabGlyph ? "gitlab" : isPool(row) ? "pool" : "adhoc";
  const node = el("a", "ctr worker-item", el("span", "mark"), el("span", "nm", row.name, newBadge(row, now) ? " " : null, newBadge(row, now)), el("span", "kd", glyph(glyphChar, glyphClass), el("span", "muted", kind)), el("span", "st-" + status[1], status[0]),
    agentLabel(row.agent), el("span", "tp", row.topic || ""),
    el("span", "ls", leaseBar(lease), el("span", "lease-text" + (lease.tone && lease.kind !== "pool" ? " " + lease.tone : ""), lease.kind === "pool" ? lease.text : lease.text.replace(" left", ""))),
    el("span", "muted", relative(row.created_at, now).replace(" ago", "")));
  node.href = workerHash(row);
  node.dataset.worker = row.name;
  if (current) node.setAttribute("aria-current", "true");
  const notable = JSON.stringify([row.topic, row.status, row.retired, row.pool_state, row.agent, lease.tone]);
  if (cached && cached.notable !== notable) node.classList.add("flash");
  tableCache.set(row.name, { node, data, notable });
  return node;
}

function workerKind(row) {
  return row.name.startsWith("gha-") ? "github" : row.provider === "gitlab" || row.name.startsWith("glci-") ? "gitlab" : isPool(row) ? (row.pool || row.kind) : row.kind || "";
}

// Column widths of the console table, dragged in the header and kept in
// this browser. Columns without one keep their default track.
const tableTracks = ["minmax(150px, 1.3fr)", "110px", "84px", "minmax(90px, .8fr)", "minmax(0, 2fr)", "130px", "64px"];
const columnWidths = {};
try { Object.assign(columnWidths, JSON.parse(localStorage.getItem("rworker.columns") || "{}")); } catch { /* Storage disabled. */ }

function applyColumnWidths(table) {
  const tracks = tableColumns.map(([, key], index) => columnWidths[key] ? columnWidths[key] + "px" : tableTracks[index]);
  table.style.setProperty("--ctr-cols", "14px " + tracks.join(" "));
}

function saveColumnWidths() {
  try { localStorage.setItem("rworker.columns", JSON.stringify(columnWidths)); } catch { /* Storage disabled. */ }
}

function columnResizer(table, cell, key, label) {
  const handle = el("span", "col-resize");
  handle.setAttribute("role", "separator");
  handle.setAttribute("aria-orientation", "vertical");
  handle.setAttribute("aria-label", "Resize the " + label.toLowerCase() + " column");
  handle.tabIndex = 0;
  const set = (width) => {
    columnWidths[key] = Math.max(40, Math.min(900, Math.round(width)));
    applyColumnWidths(table);
  };
  handle.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    handle.setPointerCapture(event.pointerId);
    handle.classList.add("dragging");
    document.body.classList.add("resizing", "col");
    const left = cell.getBoundingClientRect().left;
    const move = (moved) => set(moved.clientX - left);
    const done = () => {
      handle.removeEventListener("pointermove", move);
      handle.classList.remove("dragging");
      document.body.classList.remove("resizing", "col");
      saveColumnWidths();
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", done, { once: true });
    handle.addEventListener("pointercancel", done, { once: true });
  });
  handle.addEventListener("click", (event) => event.stopPropagation());
  handle.addEventListener("dblclick", (event) => {
    event.stopPropagation();
    delete columnWidths[key];
    saveColumnWidths();
    applyColumnWidths(table);
  });
  handle.addEventListener("keydown", (event) => {
    const step = event.shiftKey ? 40 : 10;
    const delta = { ArrowLeft: -step, ArrowRight: step }[event.key];
    if (!delta) return;
    event.preventDefault();
    set(cell.getBoundingClientRect().width + delta);
    saveColumnWidths();
  });
  return handle;
}

// Column sort for the console table: [label, key, value].
const tableColumns = [
  ["NAME", "name", (row) => row.name],
  ["KIND", "kind", (row) => workerKind(row)],
  ["STATUS", "status", (row) => row.retired ? "retired" : isPool(row) && row.pool_state ? row.pool_state : String(row.status || "").toLowerCase()],
  ["AGENT", "agent", (row) => row.agent || ""],
  ["TOPIC", "topic", (row) => row.topic || ""],
  ["LEASE", "lease", (row, now) => isPool(row) || row.retired ? Infinity : row.grace_until > now ? row.grace_until - now : row.expires_at > 0 ? row.expires_at - now : Infinity],
  ["AGE", "age", (row, now) => now - (timestamp(row.created_at) || now)],
];

function tableSorted(rows, now) {
  const sort = state.tableSort;
  const column = sort && tableColumns.find(([, key]) => key === sort.key);
  if (!column) return sortWorkers(rows, now);
  const value = column[2];
  const empty = (v) => v === "" || v == null || v === Infinity;
  return rows.slice().sort((a, b) => {
    const left = value(a, now);
    const right = value(b, now);
    if (empty(left) || empty(right)) return empty(left) - empty(right);
    const order = typeof left === "number" ? left - right : String(left).localeCompare(String(right), undefined, { numeric: true, sensitivity: "base" });
    return sort.desc ? -order : order;
  });
}

function renderWorkerTable(visible, now) {
  const table = el("div", "ctable");
  table.setAttribute("role", "list");
  applyColumnWidths(table);
  const head = el("div", "ctr head", el("span"));
  for (const [label, key] of tableColumns) {
    const active = state.tableSort?.key === key;
    const heading = button(null, () => {
      state.tableSort = active && !state.tableSort.desc ? { key, desc: true } : { key, desc: false };
      render();
    }, { cls: "sort", title: "Sort by " + label.toLowerCase() });
    heading.append(label, el("span", "arrow", active ? (state.tableSort.desc ? "↓" : "↑") : ""));
    if (active) heading.setAttribute("aria-sort", state.tableSort.desc ? "descending" : "ascending");
    const cell = el("span", "th", heading);
    cell.append(columnResizer(table, cell, key, label));
    head.append(cell);
  }
  table.append(head);
  const order = [];
  for (const row of tableSorted(visible, now).slice(0, state.limit)) {
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
      card.append(el("div", "card-head", workerTile(row), el("div", "ident", el("span", "name-line", name, newBadge(row, now)), el("span", "sub", workerSub(row))), statusPill(row)), facts);
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

// A job's own topic, or its worker's when it has none. The agent is always
// the worker's.
function jobTopic(row, worker) {
  return row.topic || worker.topic || "";
}

function jobContext(visible) {
  const workers = new Map((state.rows.workers || []).map((worker) => [worker.name, worker]));
  const sorted = visible.slice().sort((a, b) => Number(filterTests.jobs.active(b)) - Number(filterTests.jobs.active(a)));
  return { workers, sorted };
}

function jobLink(cls, row) {
  const link = el("a", cls + " job-item");
  link.href = jobHash(row);
  link.dataset.worker = row.worker;
  link.dataset.job = row.job;
  if (state.selected?.name === row.worker && state.job === row.job) link.setAttribute("aria-current", "true");
  return link;
}

function topicCell(cls, topic, worker) {
  const cell = el("span", cls, topic);
  const other = worker.topic && worker.topic !== topic ? "Worker: " + worker.topic : "";
  cell.title = [topic, other].filter(Boolean).join("\n");
  return cell;
}

function finishJobs(container, rows, total) {
  showMore(container, total);
  state.order = rows.map(jobKey);
  $("results").replaceChildren(container);
}

function renderJobs(visible) {
  // Beside the worker panel the list is narrow: two-line rows, no header.
  const compact = splitActive();
  const container = el("div", "jobs-list" + (compact ? " compact" : ""));
  const { workers, sorted } = jobContext(visible);
  if (sorted.length && !compact) {
    const head = el("div", "job-row head", el("span"), el("span", "name", "Job"), el("span", "topic", "Topic"), el("span", "agent", "Agent"), el("span", "worker", "Worker"), el("span", "state", "State"));
    head.setAttribute("aria-hidden", "true");
    container.append(head);
  }
  const shown = sorted.slice(0, state.limit);
  for (const row of shown) {
    const [tone, ic] = jobTone(row.state);
    const worker = workers.get(row.worker) || {};
    const link = jobLink("job-row" + (tone === "archived" ? " archived" : ""), row);
    if (compact) {
      link.append(icon(ic, "tone-" + (tone || "info")),
        el("span", "ident", el("span", "name", row.job), el("span", "sub", [row.worker, jobTopic(row, worker)].filter(Boolean).join(" · "))), jobPill(row.state));
      link.title = [row.worker, jobTopic(row, worker), worker.agent].filter(Boolean).join(" · ");
    } else {
      link.append(icon(ic, "tone-" + (tone || "info")), el("span", "name", row.job), topicCell("topic", jobTopic(row, worker), worker),
        el("span", "agent", agentLabel(worker.agent)), el("span", "worker", row.worker), jobPill(row.state));
    }
    container.append(link);
  }
  finishJobs(container, shown, visible.length);
}

function renderJobCards(visible) {
  const container = el("div", "cards job-cards");
  const { workers, sorted } = jobContext(visible);
  const shown = sorted.slice(0, state.limit);
  for (const row of shown) {
    const [tone, ic] = jobTone(row.state);
    const worker = workers.get(row.worker) || {};
    const card = jobLink("card job-card" + (tone === "archived" ? " archived" : ""), row);
    card.append(
      el("div", "card-head", el("span", "tile", icon(ic, "tone-" + (tone || "info"))), el("div", "ident", el("span", "name", row.job), el("span", "sub", row.worker)), jobPill(row.state)),
      el("div", "card-facts", el("span", "card-topic", jobTopic(row, worker) || "—"), el("span", null, agentLabel(worker.agent))));
    container.append(card);
  }
  finishJobs(container, shown, visible.length);
}

// Column sort for the console jobs table: [label, key, value].
const jobColumns = [
  ["JOB", "job", (row) => row.job],
  ["TOPIC", "topic", jobTopic],
  ["AGENT", "agent", (row, worker) => worker.agent || ""],
  ["WORKER", "worker", (row) => row.worker],
  ["STATE", "state", (row) => String(row.state || "")],
];

function renderJobTable(visible) {
  const table = el("div", "ctable jobs-table");
  table.setAttribute("role", "list");
  const { workers, sorted } = jobContext(visible);
  const sort = state.jobSort;
  const column = sort && jobColumns.find(([, key]) => key === sort.key);
  if (column) {
    const value = (row) => column[2](row, workers.get(row.worker) || {});
    sorted.sort((a, b) => {
      const left = value(a);
      const right = value(b);
      if (!left || !right) return Number(!left) - Number(!right);
      const order = String(left).localeCompare(String(right), undefined, { numeric: true, sensitivity: "base" });
      return sort.desc ? -order : order;
    });
  }
  const head = el("div", "ctr head", el("span"));
  for (const [label, key] of jobColumns) {
    const active = sort?.key === key;
    const heading = button(null, () => {
      state.jobSort = active && !sort.desc ? { key, desc: true } : { key, desc: false };
      render();
    }, { cls: "sort", title: "Sort by " + label.toLowerCase() });
    heading.append(label, el("span", "arrow", active ? (sort.desc ? "↓" : "↑") : ""));
    if (active) heading.setAttribute("aria-sort", sort.desc ? "descending" : "ascending");
    head.append(heading);
  }
  table.append(head);
  const shown = sorted.slice(0, state.limit);
  for (const row of shown) {
    const [tone] = jobTone(row.state);
    const worker = workers.get(row.worker) || {};
    const state_ = String(row.state || "?").toLowerCase();
    const node = jobLink("ctr" + (tone === "archived" ? " archived" : ""), row);
    node.append(el("span", "mark"), el("span", "nm", row.job), topicCell("tp", jobTopic(row, worker), worker), agentLabel(worker.agent),
      el("span", "muted", row.worker), el("span", "st-" + (tone === "busy" ? "busy" : tone === "good" ? "good" : tone === "bad" ? "bad" : "idle"), state_));
    table.append(node);
  }
  finishJobs(table, shown, visible.length);
}

function eventValue(value) {
  const key = value.toLowerCase();
  const tone = key === "exit 0" ? "good" : /^(exit |error$|failed$|failure$|shutoff$|deleted$)/.test(key) ? "bad" :
    /^(running|active|ready|succeeded|success|completed)$/.test(key) ? "good" : "busy";
  return el("strong", "tone-" + tone, value.toLowerCase());
}

function eventDetails(details) {
  if (!details) return [];
  // Deletions name who deleted the worker and why: plain text, not a state.
  if (details.startsWith("by ")) return [" · ", el("span", "event-why", details.slice(3))];
  if (details.includes(" -> ")) {
    const [before, after] = details.split(" -> ", 2);
    return [" ", eventValue(before), " → ", eventValue(after)];
  }
  const field = /^([a-z_-]+)=(.*)$/.exec(details);
  return field ? [" · ", eventValue(field[2])] : [" · ", eventValue(details)];
}

const eventKinds = {
  "worker-added": ["add", "plus", "Created"], "worker-deleted": ["del", "minus", "Deleted"],
  "worker-state": ["chg", "swap", "Worker"], "job-added": ["job", "play", "Job started"], "job-state": ["job", "jobs", "Job"],
};
function eventKind(row) {
  return eventKinds[row.kind] || ["chg", "info", row.kind.replaceAll("-", " ")];
}

// Event toasts: new events since the last look, in the bottom right corner.
// The first fetch only sets the cursor, so a reload shows nothing old.
let eventCursor = null;
let eventPoll;
function pollEvents() {
  clearTimeout(eventPoll);
  eventPoll = setTimeout(async () => {
    if (!state.authenticated) return;
    let rows;
    try { rows = await request("events"); } catch { return; }
    if (!Array.isArray(rows)) return;
    if (state.view === "events") { state.rows.events = rows; render(); }
    const latest = rows.reduce((max, row) => Math.max(max, row.id || 0), 0);
    if (eventCursor === null) { eventCursor = latest; return; }
    const fresh = rows.filter((row) => row.id > eventCursor).sort((a, b) => a.id - b.id);
    eventCursor = Math.max(eventCursor, latest);
    for (const row of fresh.slice(-3)) toastEvent(row);
    if (fresh.length > 3) toast((fresh.length - 3) + " more events", { tone: "info", actions: [{ label: "Events", run: () => { location.hash = "events"; } }] });
  }, 1500);
}

function toastEvent(row) {
  const [, , verb] = eventKind(row);
  const details = eventDetails(row.details).map((part) => (typeof part === "string" ? part : part.textContent)).join("").replace(/^ · /, "");
  const meta = [row.profile, row.topic].filter(Boolean).join(" · ");
  const [worker, job] = String(row.target).split("/");
  const failed = /^exit [1-9]/.test(details) || / → exit [1-9]/.test(details);
  toast(verb + " " + row.target + (details ? " · " + details : ""), {
    tone: failed ? "error" : "info",
    detail: meta || undefined,
    timeout: failed ? 9000 : 6000,
    actions: row.kind === "worker-deleted" ? [] : [{ label: "Open", run: () => select(workerFor(worker), job) }],
  });
}

function renderEvents(visible, now) {
  const container = el("div");
  let day = "";
  for (const row of visible.slice(0, state.limit)) {
    const label = dayLabel(row.timestamp);
    if (label !== day) {
      day = label;
      container.append(el("div", "day", label));
    }
    const [dot, ic, verb] = eventKind(row);
    const [worker, job] = String(row.target).split("/");
    const target = button(row.target, () => select(workerFor(worker), job), { cls: "target" });
    const meta = [row.profile, row.topic, row.agent, row.owner].filter(Boolean).join(" · ");
    const node = el("div", "event",
      el("span", "event-dot " + dot, icon(ic)),
      el("div", null, el("div", "event-what", el("span", null, verb + " "), target, ...eventDetails(row.details)), meta ? el("div", "event-meta", meta) : null),
      el("div", "event-when", el("div", null, timeOfDay(row.timestamp)), el("div", null, relative(row.timestamp, now))));
    // The whole row opens the event; the target keeps opening the worker.
    node.tabIndex = 0;
    node.setAttribute("role", "button");
    node.setAttribute("aria-label", verb + " " + row.target + ": details");
    node.addEventListener("click", (event) => { if (!event.target.closest(".target")) openEvent(row, verb); });
    node.addEventListener("keydown", (event) => { if (event.key === "Enter" && event.target === node) openEvent(row, verb); });
    container.append(node);
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

function profileProvider(row) {
  return ciProviders[row.provider] || "OpenStack";
}

// Split layout: one row per profile, like the worker list.
function renderProfileList(visible) {
  const list = el("div", "worker-list profile-list");
  for (const row of visible) {
    const node = el("div", "row profile-row");
    const open = el("button", "profile-open", profileTile(row), el("span", "ident",
      el("span", "name-line", el("span", "name", row.name), row.default ? el("span", "tag default", "default") : null),
      el("span", "sub", [profileProvider(row), row.flavor, row.arch, (row.image_tags || []).join(", ")].filter(Boolean).join(" · "))));
    open.type = "button";
    open.title = "Details of " + row.name;
    open.addEventListener("click", () => openProfile(row));
    const end = el("span", "profile-end", el("span", "specs", ...profileSpecs(row).map((spec) => el("span", "tag", spec))));
    if (state.authenticated) end.append(iconButton("plus", "New " + row.name + " worker", () => form("create", null, { profile: row.name }), "btn sm icon"));
    node.append(open, end);
    list.append(node);
  }
  $("results").replaceChildren(list);
}

// Console layout: a sortable table: [label, key, value].
const profileColumns = [
  ["PROFILE", "name", (row) => row.name],
  ["PROVIDER", "provider", (row) => profileProvider(row)],
  ["FLAVOR", "flavor", (row) => row.flavor || ""],
  ["ARCH", "arch", (row) => row.arch || ""],
  ["VCPU", "vcpus", (row) => row.vcpus ?? ""],
  ["RAM", "ram", (row) => row.ram_gb != null ? row.ram_gb * 1024 : row.ram_mib ?? ""],
  ["DISK", "disk", (row) => row.disk_total_bytes != null ? row.disk_total_bytes / 2 ** 30 : row.disk ?? ""],
  ["IMAGE TAGS", "tags", (row) => (row.image_tags || []).join(", ")],
];

function renderProfileTable(visible) {
  const table = el("div", "ctable profiles-table");
  const sort = state.profileSort;
  const column = sort && profileColumns.find(([, key]) => key === sort.key);
  const rows = visible.slice();
  if (column) {
    rows.sort((a, b) => {
      const left = column[2](a);
      const right = column[2](b);
      if (left === "" || right === "") return Number(left === "") - Number(right === "");
      const order = typeof left === "number" ? left - right : String(left).localeCompare(String(right), undefined, { numeric: true, sensitivity: "base" });
      return sort.desc ? -order : order;
    });
  }
  const head = el("div", "ctr head", el("span"));
  for (const [label, key] of profileColumns) {
    const active = sort?.key === key;
    const heading = button(null, () => {
      state.profileSort = active && !sort.desc ? { key, desc: true } : { key, desc: false };
      render();
    }, { cls: "sort", title: "Sort by " + label.toLowerCase() });
    heading.append(label, el("span", "arrow", active ? (sort.desc ? "↓" : "↑") : ""));
    if (active) heading.setAttribute("aria-sort", sort.desc ? "descending" : "ascending");
    head.append(heading);
  }
  table.append(head);
  for (const row of rows) {
    const node = el("button", "ctr", el("span", "mark"),
      el("span", "nm", row.name, row.default ? " " : null, row.default ? el("span", "tag default", "default") : null),
      el("span", "muted", profileProvider(row)), el("span", null, row.flavor || "—"), el("span", null, row.arch || "—"),
      el("span", null, row.vcpus ?? "—"), el("span", null, ramLabel(row) || "—"), el("span", null, diskLabel(row).replace(" disk", "") || "—"),
      el("span", "muted", (row.image_tags || []).join(", ") || "—"));
    node.type = "button";
    node.title = "Details of " + row.name;
    node.addEventListener("click", () => openProfile(row));
    table.append(node);
  }
  $("results").replaceChildren(table);
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

// Logs view: all jobs on all workers (like `rworker logs -f`), or one
// worker's or job's, with a line filter.
function logsPath() {
  const view = state.redact ? "logs" : "raw-logs";
  if (!state.logsWorker) return view;
  return view + "/" + encodeURIComponent(state.logsWorker) + (state.logsJob ? "/" + encodeURIComponent(state.logsJob) : "");
}

function startLogsStream(reset = true) {
  closeLogStream();
  if (reset) setLog("");
  if (state.follow) followLogs(logsPath());
  else liveIndicator("Paused");
}

function logsHash(worker, job) {
  return "#logs" + (worker ? "/" + encodeURIComponent(worker) + (job ? "/" + encodeURIComponent(job) : "") : "");
}

function fillLogsPickers(root) {
  const [workerPicker, jobPicker] = root.querySelectorAll(".log-tools .dropdown");
  const workers = workerOptions(true);
  if (state.logsWorker && !workers.some((option) => option.value === state.logsWorker)) {
    workers.splice(1, 0, { value: state.logsWorker, label: state.logsWorker, mono: true, sub: "not live", icon: "archive" });
  }
  workerPicker.setOptions(workers);
  workerPicker.setValue(state.logsWorker);
  if (state.logsWorker) {
    const jobs = (state.logsJobs || []).map((job) => ({ worker: state.logsWorker, ...job }));
    if (state.logsJob && !jobs.some((job) => job.job === state.logsJob)) jobs.unshift({ worker: state.logsWorker, job: state.logsJob, state: "" });
    jobPicker.setOptions(jobOptions(jobs));
    jobPicker.setValue(state.logsJob);
  } else {
    // Without a worker: any job, grouped by worker; picking one picks both.
    const live = new Set((state.rows.workers || []).filter((row) => !row.retired).map((row) => row.name));
    const jobs = (state.rows.jobs || []).filter((job) => job.state !== "archived" || live.has(job.worker));
    jobPicker.setOptions(jobOptions(jobs, { withWorker: true, allLabel: "All jobs on all workers" }));
    jobPicker.setValue("");
  }
}

function renderLogsView() {
  const results = $("results");
  const key = JSON.stringify([state.logsWorker, state.logsJob, state.redact]);
  const existing = results.querySelector(".logs-view");
  if (existing && state.logsKey === key) {
    fillLogsPickers(existing);
    return;
  }
  state.logsKey = key;
  if (state.logsWorker && !state.logsJobs) {
    request("jobs/" + encodeURIComponent(state.logsWorker)).then((jobs) => {
      state.logsJobs = jobs;
      const view = $("results").querySelector(".logs-view");
      if (view) fillLogsPickers(view);
    }).catch(() => {});
  }
  const container = el("div", "logs-view");
  const tools = el("div", "log-tools");
  const workerSelect = dropdown({ label: "Worker", searchPlaceholder: "Worker, topic, agent", onChange: (value) => { state.logsJobs = null; location.hash = logsHash(value); } });
  const jobSelect = dropdown({ label: "Job", searchPlaceholder: "Job or worker", onChange: (value) => {
    if (state.logsWorker) location.hash = logsHash(state.logsWorker, value);
    else if (value) {
      const [worker, ...job] = value.split("/");
      state.logsJobs = null;
      location.hash = logsHash(worker, job.join("/"));
    }
  } });
  const follow = button(state.follow ? "Following" : "Paused", () => setFollow(!state.follow), { cls: "btn sm follow" + (state.follow ? " on" : ""), ic: state.follow ? "pause" : "play", title: "Follow new output (F)" });
  follow.setAttribute("aria-pressed", String(state.follow));
  const toggle = (ic, title, on, change) => {
    const node = iconButton(ic, title, () => { change(); node.classList.toggle("on"); node.setAttribute("aria-pressed", String(node.classList.contains("on"))); }, "btn sm icon" + (on ? " on" : ""));
    node.setAttribute("aria-pressed", String(on));
    return node;
  };
  const frame = el("div", "log-frame" + (state.wrap ? " wrap" : ""));
  tools.append(workerSelect, jobSelect, el("span", "spacer"), follow,
    toggle("wrap", "Wrap long lines (W)", state.wrap, () => { state.wrap = !state.wrap; frame.classList.toggle("wrap", state.wrap); }),
    toggle("shield", "Redact secrets", state.redact, () => { state.redact = !state.redact; render(); }),
    iconButton("copy", "Copy log", () => copy(stripAnsi(state.logText)), "btn sm icon"),
    iconButton("download", "Download log", () => download(stripAnsi(state.logText), (state.logsWorker || "all-workers") + (state.logsJob ? "-" + state.logsJob : "") + ".log"), "btn sm icon"),
    fullscreenButton(container));
  const pre = el("pre", null, "Connecting…");
  pre.tabIndex = 0;
  const jump = button("Jump to latest", () => { pre.scrollTop = pre.scrollHeight; if (!state.follow) setFollow(true); }, { cls: "btn sm jump", ic: "down" });
  jump.hidden = true;
  pre.addEventListener("scroll", () => { jump.hidden = pre.scrollHeight - pre.clientHeight - pre.scrollTop < 12; }, { passive: true });
  frame.append(el("span", "live", "Connecting…"), pre, jump);
  container.append(tools, frame);
  results.replaceChildren(container);
  fillLogsPickers(container);
  state.logText = "";
  startLogsStream();
}

async function refresh(manual = false) {
  if (!state.authenticated || loading) return;
  loading = true;
  const view = state.view;
  const access = state.token;
  try {
    if (view === "logs") {
      state.rows.workers = await request("workers");
      if (state.logsWorker) state.logsJobs = await request("jobs/" + encodeURIComponent(state.logsWorker)).catch(() => state.logsJobs);
      else state.rows.jobs = await request("jobs").catch(() => state.rows.jobs);
      connection("Live", "online");
      if (state.view === "logs") render();
      return;
    }
    const data = await request(view);
    if (state.token !== access) return;
    state.rows[view] = Array.isArray(data) ? data : [];
    if (view !== "workers" && !state.rows.workers) {
      state.rows.workers = await request("workers");
    }
    const stale = state.inventoryStale && view === "workers";
    connection(stale ? "Cached inventory" : "Live", stale ? "stale" : "online");
    if (state.selected && detailViews.has(view)) {
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
  pollEvents();
  try { state.rows.profiles = await request("profiles"); } catch { /* Shown when the view is opened. */ }
  try { state.rows.jobs = await request("jobs"); } catch { /* Shown when the view is opened. */ }
  lastRenderKey = "";
  render();
  if (state.selected) renderDetailHead();
}

function openEvent(row, verb) {
  const [worker, job] = String(row.target).split("/");
  const when = new Date(row.timestamp * 1000);
  $("event-title").replaceChildren(verb + " ", el("span", "mono", row.target));
  const details = row.details?.startsWith("by ") ? row.details.slice(3) : row.details;
  const fields = [
    ["Event", row.kind], ["When", when.toLocaleString() + " · " + relative(row.timestamp, Date.now() / 1000)],
    [row.kind === "worker-deleted" ? "Deleted by" : "Details", details],
    ["Worker", worker], ["Job", job], ["Status", row.status], ["Profile", row.profile], ["Provider", row.provider],
    ["Agent", [row.agent, row.agent_id].filter(Boolean).join(" · ")], ["Topic", row.topic], ["Owner", row.owner], ["Event ID", row.id],
  ];
  $("event-metadata").replaceChildren(...fields.filter(([, value]) => value != null && value !== "").map(([key, value]) =>
    el("div", "kv", el("dt", null, key), el("dd", null, el("span", null, String(value))))));
  $("event-open").onclick = () => {
    $("event-dialog").close();
    select(workerFor(worker), job);
  };
  $("event-dialog").showModal();
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

// Views whose items open the worker panel beside (or below) the list.
const detailViews = new Set(["workers", "jobs"]);

function jobHash(row) {
  return "#jobs/" + encodeURIComponent(row.worker) + "/" + encodeURIComponent(row.job);
}

function jobKey(row) {
  return row.worker + "/" + row.job;
}

function sourceWorker(name) {
  return (state.rows.workers || []).find((worker) => worker.name === name);
}

function workerFor(name) {
  return sourceWorker(name) || { name };
}

function splitActive() {
  return prefs.layout !== "cards" && detailViews.has(state.view) && !narrow.matches;
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
  $("splitter").hidden = !split;
  applySplit();
  $("keybar").hidden = !(split && prefs.layout === "console" && state.authenticated);
  $("detail").hidden = !row;
  $("detail-empty").hidden = !split || Boolean(row);
  $("detail-empty-text").textContent = state.view === "jobs" ? "Select a job to follow its log." : "Select a worker to see its logs, jobs and details.";
  $("inventory").hidden = fullPage;
  $("app").classList.toggle("detail-open", fullPage);
  $("back").hidden = !fullPage;
  $("filters").hidden = fullPage;
  $("search-box").hidden = fullPage;
  if (fullPage) $("banner").hidden = true;
  $("create").hidden = !state.authenticated;
  $("fab").hidden = !state.authenticated || state.view !== "workers";
  $("heading").textContent = views[state.view];
  $("banner").dataset.key = "";
  document.title = (row?.name || views[state.view]) + " · rworker";
}

// The divider between list and worker: its position per layout, in px,
// kept in this browser.
const splitSizes = {};
try { Object.assign(splitSizes, JSON.parse(localStorage.getItem("rworker.split") || "{}")); } catch { /* Storage disabled. */ }

function applySplit() {
  const workspace = $("workspace");
  const vertical = prefs.layout === "console";
  workspace.style.removeProperty("--split-left");
  workspace.style.removeProperty("--split-top");
  const size = splitSizes[prefs.layout];
  if (!size || !workspace.classList.contains("split")) return;
  const box = workspace.getBoundingClientRect();
  const limit = (vertical ? box.height : box.width) - 220;
  const clamped = Math.max(vertical ? 110 : 260, Math.min(size, limit > 0 ? limit : size));
  workspace.style.setProperty(vertical ? "--split-top" : "--split-left", Math.round(clamped) + "px");
}

function setSplit(size) {
  if (size == null) delete splitSizes[prefs.layout];
  else splitSizes[prefs.layout] = Math.round(size);
  try { localStorage.setItem("rworker.split", JSON.stringify(splitSizes)); } catch { /* Storage disabled. */ }
  applySplit();
}

function setupSplitter() {
  const bar = $("splitter");
  const current = () => {
    const pane = $("inventory").getBoundingClientRect();
    return prefs.layout === "console" ? pane.height : pane.width;
  };
  bar.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    bar.setPointerCapture(event.pointerId);
    bar.classList.add("dragging");
    document.body.classList.add("resizing");
    const box = $("workspace").getBoundingClientRect();
    const move = (moved) => {
      splitSizes[prefs.layout] = prefs.layout === "console" ? moved.clientY - box.top : moved.clientX - box.left;
      applySplit();
    };
    const done = () => {
      bar.removeEventListener("pointermove", move);
      bar.classList.remove("dragging");
      document.body.classList.remove("resizing");
      setSplit(current());
    };
    bar.addEventListener("pointermove", move);
    bar.addEventListener("pointerup", done, { once: true });
    bar.addEventListener("pointercancel", done, { once: true });
  });
  bar.addEventListener("dblclick", () => setSplit(null));
  bar.addEventListener("keydown", (event) => {
    const step = event.shiftKey ? 80 : 24;
    const delta = { ArrowUp: -step, ArrowLeft: -step, ArrowDown: step, ArrowRight: step }[event.key];
    if (!delta) return;
    event.preventDefault();
    setSplit(current() + delta);
  });
  bar.setAttribute("aria-label", "Resize the worker list");
  addEventListener("resize", applySplit);
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
      if (key !== "logs" && state.logFullscreen) setLogFullscreen(false);
      state.tab = key;
      renderTabs();
      updateDetail(true);
    }, { cls: "tab", ic });
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-selected", String(state.tab === key));
    if (key === "jobs" && running) tab.append(el("span", "n busy", String(running)));
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
function logRoot() {
  return state.view === "logs" && !state.selected ? $("results") : $("detail-output");
}

function liveIndicator(text, tone = "") {
  const node = logRoot().querySelector(".live");
  if (!node) return;
  node.textContent = text;
  node.className = "live" + (tone ? " " + tone : "");
}

function logElements() {
  const output = logRoot();
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

let logHighlight = "";
function ansiInto(target, text, st) {
  const pattern = /\x1b\[([0-9;?]*)[ -/]*([@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[^[\]]/g;
  let last = 0;
  const fragment = document.createDocumentFragment();
  // Mark the filter's matches inside each colored chunk.
  const appendMarked = (parent, chunk) => {
    if (!logHighlight) { parent.append(chunk); return; }
    const lower = chunk.toLowerCase();
    let index = 0;
    for (let at = lower.indexOf(logHighlight); at >= 0; at = lower.indexOf(logHighlight, index)) {
      if (at > index) parent.append(chunk.slice(index, at));
      parent.append(el("mark", null, chunk.slice(at, at + logHighlight.length)));
      index = at + logHighlight.length;
    }
    if (index < chunk.length) parent.append(chunk.slice(index));
  };
  const flush = (chunk) => {
    if (!chunk) return;
    if (st.prefixSpace) {
      st.prefixSpace = false;
      if (chunk.startsWith(" ")) {
        fragment.append(el("span", "log-prefix", " "));
        chunk = chunk.slice(1);
        if (!chunk) return;
      }
    }
    const isPrefix = st.lineStart !== false && st.bold && st.fg != null && /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(chunk);
    st.lineStart = chunk.endsWith("\n");
    if (isPrefix) st.prefixSpace = true;
    const cls = [st.bold ? "b" : "", st.dim ? "d" : "", st.italic ? "it" : "", st.underline ? "u" : "", st.fg != null ? "f" + st.fg : "", isPrefix ? "log-prefix" : ""].filter(Boolean).join(" ");
    if (!cls && !st.color) { appendMarked(fragment, chunk); return; }
    const span = document.createElement("span");
    if (cls) span.className = cls;
    if (st.color) span.style.color = st.color;
    appendMarked(span, chunk);
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
    logHighlight = query;
    if (lines.length) ansiInto(pre, lines.join("\n"), {});
    else pre.textContent = "No lines match “" + state.logQuery + "”.";
    logHighlight = "";
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
  const picker = tools.querySelector(".dropdown");
  if (picker) {
    const jobs = [...(state.detailJobs || [])];
    if (state.job && !jobs.some((job) => job.job === state.job)) jobs.push({ job: state.job, state: "" });
    picker.setOptions(jobOptions(jobs));
    picker.setValue(state.job || "");
  }
}

// Full-window log: the tools and the log cover the console until toggled
// back (button or Esc). Kept across job switches and re-renders.
function fullscreenButton(target) {
  target.classList.toggle("log-fullscreen", Boolean(state.logFullscreen));
  const node = iconButton(state.logFullscreen ? "minimize" : "expand", state.logFullscreen ? "Exit full screen (Esc)" : "Full screen", () => {
    setLogFullscreen(!state.logFullscreen);
  }, "btn sm icon fullscreen" + (state.logFullscreen ? " on" : ""));
  node.setAttribute("aria-pressed", String(Boolean(state.logFullscreen)));
  return node;
}

function setLogFullscreen(enabled) {
  state.logFullscreen = enabled;
  for (const target of document.querySelectorAll(".logs-view, #detail-output")) target.classList.toggle("log-fullscreen", enabled);
  for (const node of document.querySelectorAll(".log-tools .fullscreen")) {
    node.classList.toggle("on", enabled);
    node.setAttribute("aria-pressed", String(enabled));
    node.title = enabled ? "Exit full screen (Esc)" : "Full screen";
    node.setAttribute("aria-label", node.title);
    node.replaceChildren(icon(enabled ? "minimize" : "expand"));
  }
  document.body.classList.toggle("log-fullscreen-open", enabled);
}

function buildLogPane(row) {
  const output = $("detail-output");
  const tools = el("div", "log-tools");
  const jobSelect = dropdown({ label: "Job", searchPlaceholder: "Job", onChange: (value) => select(row, value || null) });
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
  tools.append(jobSelect, el("span", "spacer"), search);
  if (!row.retired) {
    const follow = button(state.follow ? "Following" : "Paused", () => setFollow(!state.follow), { cls: "btn sm follow" + (state.follow ? " on" : ""), ic: state.follow ? "pause" : "play", title: "Follow new output (F)" });
    follow.setAttribute("aria-pressed", String(state.follow));
    tools.append(follow);
  }
  tools.append(
    toggle("Wrap", "wrap", state.wrap, () => { state.wrap = !state.wrap; output.querySelector(".log-frame")?.classList.toggle("wrap", state.wrap); }, "Wrap long lines (W)"),
    toggle("Redact", "shield", state.redact, () => { state.redact = !state.redact; updateDetail(true); }, "Redact secrets"),
    iconButton("copy", "Copy log", () => copy(stripAnsi(state.logText)), "btn sm icon"),
    iconButton("download", "Download log", () => download(stripAnsi(state.logText), `${row.name}${state.job ? "-" + state.job : ""}.log`), "btn sm icon"),
    fullscreenButton(output));
  const frame = el("div", "log-frame" + (state.wrap ? " wrap" : ""));
  const pre = el("pre", null, "Loading…");
  pre.tabIndex = 0;
  const jump = button("Jump to latest", () => { pre.scrollTop = pre.scrollHeight; if (!state.follow && !row.retired) setFollow(true); }, { cls: "btn sm jump", ic: "down" });
  jump.hidden = true;
  pre.addEventListener("scroll", () => { jump.hidden = pre.scrollHeight - pre.clientHeight - pre.scrollTop < 12; }, { passive: true });
  frame.append(live, pre, jump);
  output.replaceChildren(tools, frame);
  renderLogTools();
}

function setFollow(enabled) {
  state.follow = enabled;
  const button = logRoot().querySelector(".follow");
  if (button) {
    button.classList.toggle("on", enabled);
    button.setAttribute("aria-pressed", String(enabled));
    button.replaceChildren(icon(enabled ? "pause" : "play"), el("span", null, enabled ? "Following" : "Paused"));
  }
  if (enabled) {
    if (state.view === "logs" && !state.selected) startLogsStream();
    else updateDetail(true);
  } else {
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
        list.append(el("div", "item" + (tone === "archived" ? " archived" : ""), icon(ic, "tone-" + (tone || "info")), el("span", "ident", el("span", "name", entry.job), entry.topic ? el("span", "sub", entry.topic) : ""), jobPill(entry.state),
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

// Dropdowns: a button showing the chosen option and a searchable list with
// icons, details and groups. Options: {value, label, sub, glyph, glyphCls,
// icon, meta (text or node), group, mono}.
function optionParts(option, compact) {
  const parts = [];
  if (option.glyph) parts.push(glyph(option.glyph, option.glyphCls || ""));
  else if (option.icon) parts.push(icon(option.icon, option.iconCls || ""));
  const text = el("span", "dd-text", el("span", "dd-label" + (option.mono ? " mono" : ""), option.label ?? ""));
  if (!compact && option.sub) text.append(el("span", "dd-sub", option.sub));
  parts.push(text);
  if (!compact && option.meta) parts.push(option.meta instanceof Node ? option.meta.cloneNode(true) : el("span", "dd-meta", option.meta));
  return parts;
}

function dropdown({ label, placeholder = "", onChange, searchPlaceholder = "Search" }) {
  const trigger = el("button", "btn sm dropdown");
  trigger.type = "button";
  trigger.setAttribute("aria-haspopup", "listbox");
  trigger.setAttribute("aria-label", label);
  let options = [];
  let value = "";
  const paint = () => {
    const chosen = options.find((option) => option.value === value);
    trigger.replaceChildren(...optionParts(chosen || { label: placeholder, icon: "chev-r" }, true), icon("chev-d", "caret"));
    trigger.title = chosen?.sub ? `${chosen.label} · ${chosen.sub}` : chosen?.label || placeholder;
  };
  trigger.addEventListener("click", () => openDropdown(trigger, options, value, (chosen) => {
    value = chosen;
    paint();
    onChange(chosen);
  }, searchPlaceholder));
  trigger.setOptions = (next) => { options = next; paint(); };
  trigger.setValue = (next) => { value = next ?? ""; paint(); };
  paint();
  return trigger;
}

function openDropdown(anchor, options, current, choose, searchPlaceholder) {
  closeMenu();
  const scrim = el("div", "menu-scrim");
  const panel = el("div", "menu dropdown-panel");
  const searchable = options.length > 6;
  const input = el("input");
  input.type = "search";
  input.placeholder = searchPlaceholder;
  input.setAttribute("aria-label", searchPlaceholder);
  const list = el("div", "dd-list");
  list.setAttribute("role", "listbox");
  let shown = [];
  let active = 0;
  const highlight = () => {
    list.querySelectorAll(".dd-option").forEach((node, index) => node.setAttribute("aria-selected", String(index === active)));
    list.querySelectorAll(".dd-option")[active]?.scrollIntoView({ block: "nearest" });
  };
  const fill = () => {
    const query = input.value.trim().toLowerCase();
    shown = options.filter((option) => !query || [option.label, option.sub, option.group, option.search].filter(Boolean).join(" ").toLowerCase().includes(query));
    list.replaceChildren();
    let group;
    shown.forEach((option, index) => {
      if (option.group && option.group !== group) {
        group = option.group;
        list.append(el("div", "dd-group", group));
      }
      const node = el("button", "dd-option" + (option.value === current ? " current" : ""), ...optionParts(option, false));
      node.type = "button";
      node.setAttribute("role", "option");
      node.addEventListener("click", () => { closeMenu(); anchor.focus({ preventScroll: true }); choose(option.value); });
      node.addEventListener("mousemove", () => { if (active !== index) { active = index; highlight(); } });
      list.append(node);
    });
    if (!shown.length) list.append(el("div", "dd-empty", "Nothing matches."));
    active = Math.max(0, shown.findIndex((option) => option.value === current));
    highlight();
  };
  const keys = (event) => {
    if (event.key === "ArrowDown") { event.preventDefault(); active = Math.min(shown.length - 1, active + 1); highlight(); }
    else if (event.key === "ArrowUp") { event.preventDefault(); active = Math.max(0, active - 1); highlight(); }
    else if (event.key === "Enter") { event.preventDefault(); const option = shown[active]; if (option) { closeMenu(); anchor.focus(); choose(option.value); } }
    else if (event.key === "Escape") { event.preventDefault(); closeMenu(); anchor.focus(); }
    else if (event.key === "Tab") closeMenu();
  };
  input.addEventListener("input", fill);
  panel.addEventListener("keydown", keys);
  if (searchable) panel.append(el("div", "dd-search", icon("search"), input));
  panel.append(list);
  scrim.addEventListener("click", closeMenu);
  document.body.append(scrim, panel);
  fill();
  const box = anchor.getBoundingClientRect();
  panel.style.minWidth = Math.max(box.width, 300) + "px";
  const top = box.bottom + 6 + panel.offsetHeight > innerHeight - 8 ? Math.max(8, box.top - panel.offsetHeight - 6) : box.bottom + 6;
  panel.style.top = top + "px";
  panel.style.left = Math.min(Math.max(8, box.left), innerWidth - panel.offsetWidth - 8) + "px";
  menuState = { menu: panel, scrim };
  (searchable ? input : list.querySelector(".dd-option[aria-selected=true]") || list.querySelector(".dd-option"))?.focus();
}

function workerOptions(includeAll) {
  const now = Date.now() / 1000;
  const rows = (state.rows.workers || []).filter((row) => !row.retired);
  const options = includeAll ? [{ value: "", label: "All workers", sub: "Every job on every worker", icon: "server" }] : [];
  for (const row of sortWorkers(rows, now)) {
    const lease = leaseInfo(row, now);
    const glyphChar = workerGlyph(row);
    options.push({
      value: row.name, label: row.name, mono: true,
      sub: [row.topic, row.agent, row.owner].filter(Boolean).join(" · "),
      glyph: glyphChar, glyphCls: glyphChar === githubGlyph ? "github" : glyphChar === gitlabGlyph ? "gitlab" : isPool(row) ? "pool" : "adhoc",
      meta: lease.text, group: isPool(row) ? (row.pool || row.kind) + " pool" : "Ad-hoc",
      search: [row.profile, row.kind].filter(Boolean).join(" "),
    });
  }
  return options;
}

function jobOptions(jobs, { withWorker = false, allLabel = "All jobs" } = {}) {
  const unique = [...new Map(jobs.map((job) => [(job.worker || "") + "/" + job.job, job])).values()]
    .sort((a, b) => (withWorker ? (a.worker || "").localeCompare(b.worker || "") : 0) ||
      Number(filterTests.jobs.active(b)) - Number(filterTests.jobs.active(a)) || a.job.localeCompare(b.job));
  return [{ value: "", label: allLabel, icon: "jobs" }, ...unique.map((job) => {
    const [tone, ic] = jobTone(job.state);
    return {
      value: withWorker ? job.worker + "/" + job.job : job.job, label: job.job, mono: true,
      sub: [withWorker ? job.worker : "", job.topic, job.started_at ? "started " + relative(job.started_at) : ""].filter(Boolean).join(" · "),
      icon: ic, iconCls: "tone-" + (tone || "info"), meta: job.state ? jobPill(job.state) : null,
      group: withWorker ? job.worker : undefined,
    };
  })];
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
      field("Additional SSH public keys", "ssh_key", { value: options.ssh_key || "", kind: "textarea", required: false, placeholder: "ssh-ed25519 AAAA…", help: "One public key per line." }),
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
      field("Topic", "topic", { required: false, placeholder: "What is this job for?", attrs: { maxlength: "255" } }),
      field("Command", "command", { kind: "textarea", placeholder: "cd repo && nix develop -c cargo test", help: "Runs on the worker from the home directory. Output appears under Logs." }));
  }
  if (action === "register") {
    fields.append(field("Job name", "job"),
      field("Topic", "topic", { required: false, placeholder: "What is this job for?", attrs: { maxlength: "255" } }),
      field("Absolute log path on the worker", "path", { placeholder: "/home/rworker/build.log" }));
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
  for (const key of ["flavor", "disk", "agent_id", "ssh_key"]) if (!action[key]) delete action[key];
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
    ["create", "extend"].includes(action.action) && !/^\d[\dhm]{0,15}$/.test(action.ttl || "") ? "Enter a lifetime such as 90m, 2h or 1h30m." :
    action.action === "create" && action.ssh_key && action.ssh_key.split(/\r?\n/).some((line) => line.trim() && !/^(ssh-ed25519|ssh-rsa) [A-Za-z0-9+/]+={0,2}(?:[ \t]+[^\r\n]*)?$/.test(line.trim())) ? "Enter a valid Ed25519 or RSA SSH public key." : "";
  if (problem) {
    $("form-error").textContent = problem;
    $("form-error").hidden = false;
    (missing || (action.ssh_key && problem.includes("SSH public key") ? $("field-ssh_key") : $("field-ttl-custom")))?.focus();
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
  for (const [view, label] of Object.entries(views)) add("Go to", { workers: "server", jobs: "jobs", logs: "terminal", events: "activity", profiles: "layers" }[view], el("span", null, label), "go view " + view, () => { location.hash = view; });
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

// Phones: pull the palette down to close it (from the list only when it
// is scrolled to the top).
{
  const sheet = $("palette");
  let start = null;
  let dy = 0;
  sheet.addEventListener("touchstart", (event) => {
    const list = $("palette-list");
    start = event.touches.length === 1 && (!list.contains(event.target) || list.scrollTop <= 0) ? event.touches[0].clientY : null;
    dy = 0;
  }, { passive: true });
  sheet.addEventListener("touchmove", (event) => {
    if (start === null) return;
    dy = event.touches[0].clientY - start;
    if (dy <= 0) { sheet.style.transform = ""; return; }
    sheet.style.transform = `translateY(${dy}px)`;
  }, { passive: true });
  const finish = () => {
    if (start === null) return;
    start = null;
    sheet.style.transform = "";
    if (dy > 80) sheet.close();
  };
  sheet.addEventListener("touchend", finish, { passive: true });
  sheet.addEventListener("touchcancel", () => { dy = 0; finish(); }, { passive: true });
}

// Phones: swipe up on the tab bar opens the palette.
{
  let start = null;
  $("tabbar").addEventListener("touchstart", (event) => {
    const touch = event.touches[0];
    start = event.touches.length === 1 ? { x: touch.clientX, y: touch.clientY } : null;
  }, { passive: true });
  $("tabbar").addEventListener("touchend", (event) => {
    const touch = event.changedTouches[0];
    if (!start || !touch) return;
    const dx = touch.clientX - start.x;
    const dy = touch.clientY - start.y;
    start = null;
    if (dy < -40 && Math.abs(dy) > 2 * Math.abs(dx)) openPalette();
  }, { passive: true });
  $("tabbar").addEventListener("touchcancel", () => { start = null; }, { passive: true });
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
  [["1"], "Workers"], [["2"], "Jobs"], [["3"], "Logs"], [["4"], "Profiles"], [["5"], "Events"],
  [["N"], "New worker"], [["J"], "Next worker"], [["K"], "Previous worker"], [["Enter"], "Open the focused worker"],
  [["Esc"], "Close the worker or dialog"], [["S"], "SSH into the open worker"], [["E"], "Extend its lease"],
  [["T"], "Edit its topic"], [["D"], "Delete it"], [["L"], "Show its logs"], [["F"], "Follow or pause the log"],
  [["W"], "Wrap long log lines"], [["R"], "Refresh"], [["["], "Collapse or expand the sidebar"], [["?"], "This list"],
];
function showKeys() {
  $("keys-list").replaceChildren(...shortcuts.flatMap(([keys, text]) => [el("dt", null, ...keys.map((key) => el("kbd", null, key))), el("dd", null, text)]));
  $("keys").showModal();
}

function moveSelection(delta) {
  if (state.view === "jobs") {
    if (state.selected && state.job) {
      const index = state.order.indexOf(state.selected.name + "/" + state.job);
      const next = state.order[index < 0 ? 0 : index + delta];
      if (next) {
        const [worker, ...job] = next.split("/");
        location.hash = jobHash({ worker, job: job.join("/") });
      }
      return;
    }
  } else if (state.view !== "workers") return;
  if (state.view === "workers" && state.selected) {
    const index = state.order.indexOf(state.selected.name);
    const next = state.order[index < 0 ? 0 : index + delta];
    if (next) select(workerFor(next));
    return;
  }
  const items = [...document.querySelectorAll("#results .worker-item, #results .job-item, #results .card .name, #results .pool-member")];
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
  if (event.key === "Escape" && state.logFullscreen) {
    event.preventDefault();
    setLogFullscreen(false);
    return;
  }
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
    "[": toggleRail,
    "1": () => { location.hash = "workers"; }, "2": () => { location.hash = "jobs"; },
    "3": () => { location.hash = "logs"; }, "4": () => { location.hash = "profiles"; }, "5": () => { location.hash = "events"; },
    n: () => form("create"), j: () => moveSelection(1), k: () => moveSelection(-1),
    r: () => refresh(true),
    Escape: () => { if (row) location.hash = state.view; },
    s: () => live && openTerminal(row),
    e: () => live && !isPool(row) && form("extend", row),
    t: () => live && editTopic(),
    d: () => live && form("delete", row),
    l: () => { if (row) { state.tab = "logs"; renderTabs(); updateDetail(true); } },
    f: () => ((live && state.tab === "logs") || (state.view === "logs" && !row)) && setFollow(!state.follow),
    w: () => ((row && state.tab === "logs") || state.view === "logs") && logRoot().querySelector('[aria-label^="Wrap"]')?.click(),
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
    if (data === "jobs") refreshJobs();
    if (data === "updated" || data === "jobs") pollEvents();
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
    if (state.pendingCreate) {
      state.pendingCreate = false;
      form("create");
    }
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
$("rail-toggle").addEventListener("click", toggleRail);
applyPrefs();
$("settings-mobile").addEventListener("click", openSettings);
$("settings-close").addEventListener("click", () => $("settings").close());
$("keys-close").addEventListener("click", () => $("keys").close());
$("terminal-close").addEventListener("click", () => $("terminal-dialog").close());
$("profile-close").addEventListener("click", () => $("profile-dialog").close());
$("event-close").addEventListener("click", () => $("event-dialog").close());
$("cancel").addEventListener("click", () => $("dialog").close());
$("dismiss").addEventListener("click", () => $("dialog").close());
// Phones show the search as an icon until it is used.
function syncSearchOpen() {
  const input = $("search");
  $("toolbar").classList.toggle("search-open", document.activeElement === input || input.value !== "");
}
$("search").addEventListener("focus", syncSearchOpen);
$("search").addEventListener("blur", syncSearchOpen);

$("search").addEventListener("input", () => {
  if (state.view === "logs" && !state.selected) {
    state.logQuery = $("search").value;
    setLog(state.logText, { keepEnd: false });
    return;
  }
  state.limit = 200;
  render();
});
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
// Job lists follow the "jobs" messages of the update socket; this timer
// only catches up after a missed message or a reconnect.
async function refreshJobs() {
  if (!state.authenticated) return;
  try {
    state.rows.jobs = await request("jobs");
    renderNav(Date.now() / 1000);
    if (state.view === "jobs") render();
    if (state.view === "logs" && !state.selected) {
      if (state.logsWorker) state.logsJobs = await request("jobs/" + encodeURIComponent(state.logsWorker));
      const view = $("results").querySelector(".logs-view");
      if (view) fillLogsPickers(view);
    }
  } catch { /* Shown when the Jobs page is opened. */ }
  if (state.selected) loadDetailJobs(state.selected);
}
setInterval(() => { if (!document.hidden) refreshJobs(); }, 60000);
setInterval(() => {
  if (!state.authenticated || document.hidden || !state.follow || logSocket) return;
  if (state.selected && state.tab === "logs" && !state.selected.retired) updateDetail();
  else if (state.view === "logs" && !state.selected && Date.now() >= logRetryAt) followLogs(logsPath());
}, 2500);
$("toolbar").hidden = true;
setupSplitter();
if ("serviceWorker" in navigator) navigator.serviceWorker.register("/assets/sw.js", { scope: "/" }).catch(() => {});
navigate();
let savedToken = "";
try { savedToken = sessionStorage.getItem("rworker.operator") || ""; } catch { /* Storage disabled. */ }
openConsole(savedToken, true);
