"use strict";

const $ = (id) => document.getElementById(id);
const views = { workers: "Workers", jobs: "Jobs", events: "Events", profiles: "Profiles" };
const state = {
  token: "", authenticated: false, view: "workers", rows: {}, sort: {}, selected: null, job: null, tab: "logs",
  workerFilter: "live", showFinished: false, redact: true, follow: true, limit: 200,
};
let stream;
let logSocket;
let logRetryAt = 0;
let loading = false;
let detailRequest = 0;
let detailLoading = false;
let currentAction;

function element(tag, value, className) {
  const node = document.createElement(tag);
  if (value != null) node.textContent = String(value);
  if (className) node.className = className;
  return node;
}

function button(label, onClick, className) {
  const node = element("button", label, className);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

function notice(message, error = false) {
  $("notice").textContent = message;
  $("notice").hidden = !message;
  $("notice").classList.toggle("error", error);
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
  if ($("terminal-dialog").open) $("terminal-dialog").close();
  state.authenticated = false;
  state.token = "";
  try { sessionStorage.removeItem("rworker.operator"); } catch { /* Storage disabled. */ }
  state.rows = {};
  state.selected = null;
  detailRequest++;
  stream?.close();
  stream = undefined;
  $("console").hidden = true;
  $("login").hidden = false;
  $("lock").hidden = true;
  $("sign-out").hidden = true;
  $("create").hidden = true;
  $("results").replaceChildren();
  $("detail-output").replaceChildren();
  if ($("dialog").open) $("dialog").close();
  connection("Locked");
}

function navigate() {
  const [requested, worker, job] = location.hash.slice(1).split("/");
  state.view = Object.hasOwn(views, requested) ? requested : "workers";
  state.selected = null;
  state.limit = 200;
  $("search").value = "";
  document.querySelectorAll("[data-view]").forEach((link) => {
    if (link.dataset.view === state.view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  $("search").placeholder = "Filter " + state.view;
  $("create").hidden = !state.authenticated;
  $("extra-toggle").hidden = state.view !== "jobs";
  $("extra").checked = state.showFinished;
  detailPanel();
  render();
  if (state.authenticated && requested === "workers" && worker) {
    try {
      openWorker(workerFor(decodeURIComponent(worker)), job ? decodeURIComponent(job) : null);
    } catch (error) {
      notice(error.message, true);
    }
  }
  refresh();
}

const agentIcons = new Map([
  ["claude", ["\uec82", "claude"]], ["codex", ["\uec81", "codex"]],
  ["shell", ["\uea85", ""]], ["nixbot", ["\uf313", "nix"]], ["nix build", ["\uf313", "nix"]],
  ["copilot", ["\uec1e", ""]], ["gemini", ["\uec10", ""]], ["agy", ["\uec10", ""]],
  ["circus", ["\u{f0508}", ""]],
  ["circus-qemu", ["\u{f0508}", ""]],
  ["antigravity", ["\uec10", ""]], ["browser-daemon", ["\u{f02a0}", ""]],
]);

// Worker glyphs: the `icon` of the worker's Glance image (rworker_icon) when
// the bundled font has it, else one for its kind. Keep in sync with
// scripts/web-icon-font.sh, which builds agents.woff2 from these.
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

function profileIcon(row, className = "agent-icon") {
  const providerClass = `profile-icon-${Object.hasOwn(ciProviders, row.provider) ? row.provider : "openstack"}`;
  const icon = element("span", profileGlyph(row), `${className} ${providerClass}`);
  icon.setAttribute("aria-hidden", "true");
  return icon;
}

function workerName(row) {
  const link = element("a", null, "worker-link");
  link.href = workerHash(row);
  const icon = workerGlyph(row);
  const glyph = element("span", icon, `agent-icon worker-icon${icon === gitlabGlyph ? " profile-icon-gitlab" : ""}`);
  glyph.setAttribute("aria-hidden", "true");
  const identity = element("span", row.name, "worker-identity");
  const label = row.label || (isPool(row) ? row.kind + " pool" : "");
  if (label) identity.append(element("span", label, "secondary"));
  const context = [row.topic, row.agent].filter(Boolean).join(" · ");
  if (context) identity.append(element("span", context, "secondary worker-context"));
  link.append(glyph, identity);
  return link;
}

function agentLabel(agent) {
  if (!agent) return "—";
  const label = element("span", null, "agent");
  const icon = agentIcons.get(agent);
  if (icon) {
    const glyph = element("span", icon[0], "agent-icon " + icon[1]);
    glyph.setAttribute("aria-hidden", "true");
    label.append(glyph);
  }
  label.append(agent);
  return label;
}

function badge(value) {
  const text = String(value || "Unknown");
  const key = text.toLowerCase();
  const kind = /^(active|ready|running)$/.test(key) ? "good" :
    /^(build|boot|reboot|deleting|pending)/.test(key) ? "busy" :
    /^(error|failed|exit [1-9])/.test(key) ? "bad" : "";
  return element("span", text, "badge" + (kind ? " " + kind : ""));
}

function eventLabel(row) {
  const kind = row.kind;
  const tone = kind.endsWith("-added") ? "good" : kind.endsWith("-deleted") ? "bad" :
    kind === "job-state" ? "busy" : "info";
  const label = element("div");
  label.append(element("strong", kind.replaceAll("-", " "), "event-tone " + tone));
  if (!row.details) return label;
  const detail = element("span", null, "secondary event-detail");
  const addValue = (value) => {
    const key = value.toLowerCase();
    const color = key === "exit 0" ? "" : /^(exit |error$|failed$|failure$|shutoff$|deleted$)/.test(key) ? "bad" :
      /^(running|active|ready|succeeded|success|completed)$/.test(key) ? "good" : "busy";
    detail.append(element("span", value, "event-tone " + color));
  };
  if (row.details.includes(" -> ")) {
    const [before, after] = row.details.split(" -> ", 2);
    addValue(before);
    detail.append(" → ");
    addValue(after);
  } else {
    const field = /^([a-z_-]+=)(.*)$/.exec(row.details);
    if (field) {
      detail.append(field[1]);
      addValue(field[2]);
    } else {
      addValue(row.details);
    }
  }
  label.append(detail);
  return label;
}

function duration(seconds) {
  if (seconds < 0) return "expired";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return minutes + "m";
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours + "h" + (minutes % 60) + "m";
  return Math.floor(hours / 24) + "d" + (hours % 24) + "h";
}

function timestamp(value) {
  return typeof value === "number" ? value : Date.parse(value) / 1000;
}

function workerAge(created, now) {
  const seconds = timestamp(created);
  return Number.isFinite(seconds) ? duration(now - seconds) : "—";
}

function workerExpiry(row, now) {
  let label = "—";
  let tone = "";
  if (isPool(row) && row.pool_state) {
    label = row.pool_state;
    if (label === "idle" && row.last_busy > 0) {
      label += " " + duration(now - row.last_busy);
      tone = " idle";
    }
  } else if (row.grace_until > now) {
    label = "grace " + duration(row.grace_until - now);
    tone = " urgent";
  } else if (row.expires_at > 0) {
    const remaining = row.expires_at - now;
    label = duration(remaining);
    if (remaining < 0) tone = " urgent";
    else if (remaining < leaseWarningSeconds) tone = " soon";
  }
  return element("span", label, "worker-expiry" + tone);
}

function clock(value) {
  if (!value) return "—";
  const date = new Date(typeof value === "number" ? value * 1000 : value);
  return Number.isNaN(date.valueOf()) ? "—" : date.toLocaleString();
}

const sortFields = {
  workers: [
    (row) => row.name, (row) => row.retired ? "Retired" : row.status, (row) => row.agent,
    (row) => row.topic,
    (row, now) => isPool(row) && row.pool_state ? null :
      row.grace_until > now ? Number(row.grace_until) : Number(row.expires_at) || null,
    (row) => -timestamp(row.created_at),
  ],
  jobs: [(row) => row.job, (row) => row.state],
  events: [(row) => row.kind, (row) => row.target, (row) => timestamp(row.timestamp)],
  profiles: [(row) => row.name, (row) => row.vcpus,
    (row) => row.ram_gb == null ? row.ram_mib : row.ram_gb * 1e9 / 2 ** 20,
    (row) => row.disk_total_bytes == null ? Number(row.disk) || null : row.disk_total_bytes / 2 ** 30, (row) => row.flavor, (row) => (row.image_tags || []).join(", ")],
};
const sortLabels = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function compareSortValues(left, right, descending) {
  const empty = (value) => value == null || value === "" || Number.isNaN(value);
  if (empty(left)) return empty(right) ? 0 : 1;
  if (empty(right)) return -1;
  const order = typeof left === "number" && typeof right === "number" ?
    left - right : sortLabels.compare(left, right);
  return descending ? -order : order;
}

function table(columns, rows, view) {
  const node = element("table");
  const head = node.createTHead().insertRow();
  for (const [index, [label, optional, sortable = true]] of columns.entries()) {
    const cell = element("th", view && sortable ? null : label, optional ? "optional" : "");
    cell.scope = "col";
    if (view && sortable) {
      const sort = state.sort[view];
      const active = sort?.index === index;
      const heading = button(label, () => {
        state.sort[view] = { index, descending: active && !sort.descending };
        render();
      }, "sort-heading");
      heading.dataset.sort = String(index);
      const arrow = element("span", active ? (sort.descending ? "↓" : "↑") : "↕", "sort-arrow");
      arrow.setAttribute("aria-hidden", "true");
      heading.append(arrow);
      cell.append(heading);
      if (active) cell.setAttribute("aria-sort", sort.descending ? "descending" : "ascending");
    }
    head.append(cell);
  }
  const tbody = node.createTBody();
  for (const row of rows) {
    const tr = tbody.insertRow();
    if (row.selected) tr.className = "selected";
    row.cells.forEach((value, index) => {
      const cell = tr.insertCell();
      if (columns[index][1]) cell.className = "optional";
      if (value instanceof Node) cell.append(value);
      else cell.textContent = value ?? "—";
    });
  }
  return node;
}

function named(title, secondary, click) {
  const node = element("div");
  node.append(click ? button(title, click, "row-link") : element("strong", title));
  if (secondary) node.append(element("span", secondary, "secondary"));
  return node;
}

function workerHash(row, job) {
  return "#workers/" + encodeURIComponent(row.name) + (job ? "/" + encodeURIComponent(job) : "");
}

function selected(row, job) {
  location.hash = workerHash(row, job);
}

function openWorker(row, job) {
  state.selected = row;
  state.job = job;
  state.tab = "logs";
  state.follow = !row.retired;
  detailPanel();
  updateDetail(true);
  window.scrollTo(0, 0);
  $("heading").focus({ preventScroll: true });
}

function sourceWorker(name) {
  return (state.rows.workers || []).find((worker) => worker.name === name);
}
function workerFor(name) {
  return sourceWorker(name) || { name };
}

const leaseWarningSeconds = 15 * 60;
const workerFilters = {
  live: (row) => !row.retired,
  pool: (row) => !row.retired && isPool(row),
  expiring: (row, now) => !row.retired && !(isPool(row) && row.pool_state) &&
    (row.grace_until > now || (row.expires_at > 0 && row.expires_at - now < leaseWarningSeconds)),
  retired: (row) => Boolean(row.retired),
};
const filterButtons = [...document.querySelectorAll("[data-worker-filter]")];

function renderWorkerFilters(source, now) {
  for (const control of filterButtons) {
    const key = control.dataset.workerFilter;
    const count = state.rows.workers ? source.filter((row) => workerFilters[key](row, now)).length : "—";
    control.querySelector("strong").textContent = count;
    control.setAttribute("aria-pressed", String(key === state.workerFilter));
  }
}

function render() {
  const source = state.rows[state.view] || [];
  const results = $("results");
  const search = $("search").value.trim().toLowerCase();
  let visible = source.filter((row) => !search || JSON.stringify(row).toLowerCase().includes(search));
  const now = Date.now() / 1000;
  if (state.view === "workers") {
    renderWorkerFilters(source, now);
    visible = visible.filter((row) => workerFilters[state.workerFilter](row, now));
  }
  if (state.view === "jobs" && !state.showFinished) {
    visible = visible.filter((row) => row.state !== "archived" && !String(row.state).startsWith("exit "));
  }
  if (state.view === "events") visible.reverse();
  const sort = state.sort[state.view];
  if (sort) {
    const value = sortFields[state.view][sort.index];
    visible.sort((left, right) => compareSortValues(value(left, now), value(right, now), sort.descending));
  }
  const count = visible.length;
  visible = visible.slice(0, state.limit);

  const counter = $("results-count");
  counter.hidden = !visible.length;
  counter.textContent = count > visible.length ? `Showing ${visible.length} of ${count}` : `${count} ${count === 1 ? "result" : "results"}`;
  if (!visible.length) {
    const workerEmpty = {
      live: "No live workers.",
      pool: "No live pool workers.",
      expiring: "No leases expiring in the next 15 minutes or overdue.",
      retired: "No retired workers.",
    };
    const message = !state.rows[state.view] ? "Loading " + state.view + "…" :
      search ? "No results match your search in this view." :
        state.view === "workers" ? workerEmpty[state.workerFilter] :
        state.view === "jobs" && !state.showFinished && source.length ? "No active jobs. Show finished jobs to see earlier runs." :
          "No " + state.view + " to display.";
    results.replaceChildren(element("div", message, "empty"));
    return;
  }
  let columns;
  let rows;
  switch (state.view) {
    case "workers": {
      columns = [["Name"], ["Status"], ["Agent"], ["Topic"], ["Lease / pool"], ["Age"]];
      rows = visible.map((row) => ({
        selected: state.selected?.name === row.name,
        cells: [workerName(row), badge(row.retired ? "Retired" : row.status),
          agentLabel(row.agent), row.topic || "—", workerExpiry(row, now),
          element("span", workerAge(row.created_at, now), "worker-age")],
      }));
      break;
    }
    case "jobs":
      columns = [["Job"], ["State"]];
      rows = visible.map((row) => ({
        selected: state.selected?.name === row.worker && state.job === row.job,
        cells: [named(row.job, row.worker, () => selected(workerFor(row.worker), row.job)), badge(row.state)],
      }));
      break;
    case "events":
      columns = [["Event"], ["Target"], ["Time"]];
      rows = visible.map((row) => ({
        cells: [eventLabel(row), element("span", row.target, "event-tone info"), clock(row.timestamp)],
      }));
      break;
    default:
      columns = [["Profile"], ["vCPUs"], ["RAM"], ["Disk"], ["Flavor", true], ["Image tags", true]];
      if (state.authenticated) columns.push(["Actions", false, false]);
      rows = visible.map((row) => {
        const tags = (row.image_tags || []).join(", ");
        const profile = named(row.name, row.default ? "Default" : "", () => openProfile(row));
        if (row.flavor) profile.append(element("span", row.flavor, "secondary profile-context"));
        if (tags) profile.append(element("span", tags, "secondary profile-context"));
        const profileName = element("div", null, "profile-name");
        profileName.append(profileIcon(row, "agent-icon worker-icon"), profile);
        const ram = row.ram_gb != null ? row.ram_gb + " GB" : row.ram_mib == null ? "—" :
          row.ram_mib % 1024 === 0 ? row.ram_mib / 1024 + " GiB" : row.ram_mib + " MiB";
        const disk = row.disk_total_bytes != null ? `${(row.disk_total_bytes / 2 ** 30).toFixed(1)} GiB` :
          row.disk ? row.disk + " GiB" : "—";
        const cells = [profileName, row.vcpus, ram, disk, row.flavor, tags || "—"];
        if (state.authenticated) {
          const create = button("+ New worker", () => form("create", null, row.name), "profile-create-action");
          create.setAttribute("aria-label", `New worker from ${row.name}`);
          cells.push(create);
        }
        return { cells };
      });
  }
  const previous = results.querySelector(`[data-view-table="${state.view}"]`);
  const scrollLeft = previous?.scrollLeft ?? 0;
  const focused = previous?.contains(document.activeElement) ? document.activeElement : null;
  const focusedSort = focused?.dataset.sort;
  const focusedWorker = focused?.classList.contains("worker-link") ? focused.hash : null;
  const scroller = element("div", null, state.view === "workers" ? "table-scroll worker-table" : "table-scroll");
  scroller.dataset.viewTable = state.view;
  scroller.append(table(columns, rows, state.view));
  results.replaceChildren(scroller);
  scroller.scrollLeft = scrollLeft;
  if (focusedSort !== undefined) scroller.querySelector(`[data-sort="${focusedSort}"]`)?.focus({preventScroll: true});
  if (focusedWorker) {
    [...scroller.querySelectorAll(".worker-link")].find((link) => link.hash === focusedWorker)?.focus({ preventScroll: true });
  }
  if (count > visible.length) {
    const more = element("div", null, "more");
    more.append(button("Show more", () => { state.limit += 200; render(); }));
    results.append(more);
  }
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
    connection(stale ? "Cached inventory" : "Connected", stale ? "stale" : "online");
    if (view === state.view) {
      if (state.selected && view === "workers") {
        const current = sourceWorker(state.selected.name);
        if (current && ["status", "owner", "retired", "expires_at"].some((field) => current[field] !== state.selected[field])) {
          const retired = Boolean(current.retired) !== Boolean(state.selected.retired);
          state.selected = current;
          if (retired) state.follow = !current.retired;
          detailPanel();
          if (retired) updateDetail(true);
        }
      }
      render();
    }
  } catch (error) {
    if (state.authenticated) connection("Disconnected", "error");
    if (manual || !state.rows[view]) notice(error.message, true);
  } finally {
    loading = false;
    if (state.authenticated && state.view !== view) refresh();
  }
}

async function openProfile(row) {
  const dialog = $("profile-dialog");
  const status = $("profile-status");
  const metadata = $("profile-metadata");
  const title = $("profile-title");
  title.replaceChildren(profileIcon(row), " " + row.name);
  $("profile-create").onclick = () => {
    dialog.close();
    form("create", null, row.name);
  };
  const show = (profile) => {
    const ciProvider = ciProviders[profile.provider];
    const fields = [
      ["Provider", ciProvider || "OpenStack"],
      [ciProvider ? "Runner" : "Flavor", profile.flavor], ["Architecture", profile.system],
      ["vCPUs", profile.vcpus], ["RAM", profile.ram_gb != null ? profile.ram_gb + " GB" :
        profile.ram_mib != null ? (profile.ram_mib * 2 ** 20 / 1e9).toFixed(1) + " GB" : null],
      ["Disk", profile.disk_total_bytes != null ? (profile.disk_total_bytes / 1e9).toFixed(1) + " GB" :
        profile.disk != null ? (profile.disk * 2 ** 30 / 1e9).toFixed(1) + " GB" : null],
      ["Image tags", profile.image_tags?.join(", ")],
      ["Image name", profile.image_name || profile.image_status],
      ["Image ID", profile.image_id], ["Image created", profile.image_created_at],
    ];
    metadata.replaceChildren(...fields.filter(([, value]) => value != null && value !== "").map(([key, value]) => {
      const field = element("div");
      field.append(element("dt", key), element("dd", value));
      return field;
    }));
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

const actionIcons = {
  ssh: "m4 6 5 5-5 5m9 0h7",
  start: "M8 5v14l11-7z",
  more: "M5 12h.01M12 12h.01M19 12h.01",
  extend: "M12 8v4l3 2m6-2a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z",
  topic: "m14 6 4 4M5 19l1-5L16 4l4 4-10 10z",
  claim: "M5 21v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3m-2-14a4 4 0 1 1-8 0 4 4 0 0 1 8 0z",
  register: "M6 3h8l4 4v14H6zm8 0v5h4M9 13h6m-6 4h6",
  "renew-token": "M16 10a4 4 0 1 0-4-4 4 4 0 0 0 4 4zm-3 0-8 8v3h3v-3h3v-3l4-5",
  copy: "M8 8h12v12H8zM4 16V4h12",
  delete: "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13m-8 4v5m4-5v5",
};

function actionIcon(name) {
  const namespace = "http://www.w3.org/2000/svg";
  const icon = document.createElementNS(namespace, "svg");
  icon.setAttribute("viewBox", "0 0 24 24");
  icon.setAttribute("aria-hidden", "true");
  icon.classList.add("action-icon");
  const path = document.createElementNS(namespace, "path");
  path.setAttribute("d", actionIcons[name]);
  icon.append(path);
  return icon;
}

function workerAction(label, icon, click, variant = "") {
  const control = button(label, click, "worker-action" + (variant ? " " + variant : ""));
  control.prepend(actionIcon(icon));
  return control;
}

function detailPanel() {
  const row = state.selected;
  $("detail").hidden = !row;
  $("inventory").hidden = Boolean(row);
  $("summary").hidden = Boolean(row) || state.view !== "workers";
  $("create").hidden = Boolean(row) || !state.authenticated;
  $("worker-back").hidden = !row;
  $("heading").textContent = row?.name || views[state.view];
  document.title = $("heading").textContent + " · rworker";
  if (!row) {
    closeLogStream();
    detailRequest++;
    detailLoading = false;
    $("detail-output").replaceChildren();
    return;
  }
  $("detail-state").replaceChildren(badge(row.retired ? "Retired" : row.status));
  $("metadata").replaceChildren();
  for (const [key, value] of [["Owner", row.owner], ["Profile", row.profile], ["Agent", row.agent], ["Expires", clock(row.expires_at)]]) {
    if (!value) continue;
    const datum = element("div");
    const description = element("dd", key === "Agent" ? null : value);
    if (key === "Agent") description.append(agentLabel(value));
    if (key === "Profile") {
      description.classList.add("profile-value");
      description.prepend(profileIcon(row));
    }
    datum.append(element("dt", key), description);
    $("metadata").append(datum);
  }

  const actions = $("worker-actions");
  actions.replaceChildren();
  if (!row.retired) {
    actions.append(workerAction("SSH", "ssh", () => openTerminal(row), "primary"),
      workerAction("Start job", "start", () => form("start", row)));
    const menu = element("details", null, "worker-action-menu");
    const trigger = element("summary", null, "worker-action");
    trigger.append(actionIcon("more"), "More");
    trigger.setAttribute("aria-label", "More worker actions");
    const items = element("div", null, "worker-action-items");
    for (const [label, action] of [["Extend", "extend"], ["Edit topic", "topic"], ["Claim", "claim"],
      ["Register log", "register"], ["Renew token", "renew-token"]]) {
      items.append(workerAction(label, action, () => { menu.open = false; form(action, row); }));
    }
    items.append(workerAction("Copy SSH command", "copy", () => {
      menu.open = false;
      copy("rworker ssh " + row.name);
    }), workerAction("Copy builder entry", "copy", async () => {
        menu.open = false;
        try { await copy(await request("builder/" + encodeURIComponent(row.name))); }
        catch (error) { notice(error.message, true); }
      }), workerAction("Delete worker", "delete", () => {
        menu.open = false;
        form("delete", row);
      }, "destructive"));
    menu.append(trigger, items);
    menu.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && menu.open) {
        menu.open = false;
        trigger.focus();
      }
    });
    actions.append(menu);
  }

  const tabs = $("detail-tabs");
  tabs.replaceChildren();
  for (const [key, title] of [["logs", "Logs"], ["jobs", "Jobs"], ["inspect", "Inspect"], ["processes", "Processes"]]) {
    if (row.retired && !["logs", "jobs", "inspect"].includes(key)) continue;
    const tab = button(title, () => {
      state.tab = key;
      detailPanel();
      updateDetail(true);
    });
    tab.setAttribute("aria-pressed", String(state.tab === key));
    tabs.append(tab);
  }
}

function openTerminal(row) {
  const dialog = $("terminal-dialog");
  const screen = $("terminal-screen");
  const status = $("terminal-status");
  const terminal = new Terminal({
    fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
    cursorBlink: true,
    scrollback: 2000,
    theme: { background: "#101419", foreground: "#e8ecf5", cursor: "#9dcfff" },
  });
  const fit = new FitAddon.FitAddon();
  terminal.loadAddon(fit);
  $("terminal-title").textContent = "SSH · " + row.name;
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
  terminal.onData(data => input(encode.encode(data)));
  terminal.onBinary(data => input(Uint8Array.from(data, char => char.charCodeAt(0))));
  terminal.onResize(() => {
    if (ready && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(dimensions()));
  });
  const resize = new ResizeObserver(() => fit.fit());
  resize.observe(screen);
  dialog.addEventListener("close", () => {
    resize.disconnect();
    socket.onopen = socket.onmessage = socket.onerror = socket.onclose = null;
    socket.close();
    terminal.dispose();
  }, { once: true });
  terminal.focus();
}

$("terminal-close").addEventListener("click", () => $("terminal-dialog").close());

function checkbox(label, checked, change) {
  const node = element("label", null, "toggle");
  const input = element("input");
  input.type = "checkbox";
  input.checked = checked;
  input.addEventListener("change", () => change(input.checked));
  node.append(input, element("span", label));
  return node;
}

function stripAnsi(value) {
  return value.replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "");
}

function closeLogStream() {
  const socket = logSocket;
  logSocket = undefined;
  logRetryAt = 0;
  socket?.close();
}

function followLogs(path, content, indicator) {
  if (logSocket || Date.now() < logRetryAt) return;
  indicator.textContent = "Connecting…";
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
    const atEnd = !buffer || content.scrollHeight - content.clientHeight - content.scrollTop < 8;
    if (typeof data === "string") {
      buffer = JSON.parse(data).snapshot;
      indicator.textContent = "Cached";
    } else {
      buffer = (buffer + decoder.decode(data, {stream: true})).slice(-262144);
      indicator.textContent = "Live";
    }
    content.textContent = stripAnsi(buffer);
    if (atEnd) content.scrollTop = content.scrollHeight;
  };
  socket.onclose = () => {
    if (logSocket !== socket) return;
    logSocket = undefined;
    logRetryAt = Date.now() + 5000;
    indicator.textContent = "Reconnecting…";
  };
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
    destination.replaceChildren(element("p", "Loading…", "empty"));
  }
  try {
    let data;
    if (tab === "logs") {
      const live = !row.retired && state.follow;
      const useArchive = row.retired;
      const endpoint = useArchive ? (state.redact ? "archived-logs" : "raw-archive") : (state.redact ? "logs" : "raw-logs");
      if (live && Date.now() >= logRetryAt) {
        data = reset ? "" : destination.querySelector("pre")?.textContent || "";
      } else {
        data = await request(endpoint + "/" + target + (job ? "/" + encodeURIComponent(job) : "")).catch((error) => {
          if (!live) throw error;
          return destination.querySelector("pre")?.textContent || "";
        });
      }
    } else if (row.retired && tab === "inspect") {
      data = row;
    } else {
      data = await request(tab + "/" + target);
    }
    if (id !== detailRequest) return;
    if (tab === "jobs") {
      destination.replaceChildren(data.length ? table([["Job"], ["State"]], data.map((entry) => ({
        cells: [named(entry.job, "", () => selected(row, entry.job)), badge(entry.state)],
      }))) : element("p", "No jobs on this worker.", "empty"));
      return;
    }
    if (tab === "processes" && data.trim().split("\n").length <= 1) {
      destination.replaceChildren(element("p", "No running job processes.", "empty"));
      return;
    }
    const old = destination.querySelector("pre");
    const stayAtEnd = !old || old.scrollHeight - old.clientHeight - old.scrollTop < 8;
    const previousScroll = old?.scrollTop || 0;
    const options = element("div", null, "detail-options");
    const indicator = element("small", "Snapshot", "secondary");
    if (tab === "logs") {
      options.append(checkbox("Redact secrets", state.redact, (enabled) => { state.redact = enabled; updateDetail(true); }));
      if (!row.retired) {
        options.append(checkbox("Follow", state.follow, (enabled) => {
          state.follow = enabled;
          if (enabled) updateDetail(true);
          else { closeLogStream(); indicator.textContent = "Snapshot"; }
        }), indicator);
      }
      if (job) options.append(button("All jobs", () => selected(row)));
    } else if (tab === "inspect" && !row.retired) {
      options.append(button("Load boot metadata", async () => {
        try {
          const metadata = await request("boot/" + target);
          if (id === detailRequest) destination.querySelector("pre").textContent = JSON.stringify(metadata, null, 2);
        } catch (error) { notice(error.message, true); }
      }));
    }
    const content = element("pre", typeof data === "string" ? (stripAnsi(data) || "No output yet.") : JSON.stringify(data, null, 2));
    destination.replaceChildren(options, content);
    if (state.follow && stayAtEnd && tab === "logs") content.scrollTop = content.scrollHeight;
    else content.scrollTop = previousScroll;
    if (tab === "logs" && !row.retired && state.follow) {
      const view = state.redact ? "logs" : "raw-logs";
      followLogs(view + "/" + target + (job ? "/" + encodeURIComponent(job) : ""), content, indicator);
    }
  } catch (error) {
    if (id === detailRequest) {
      if (tab === "processes" && state.authenticated) {
        const unavailable = element("div", null, "empty process-unavailable");
        unavailable.append(element("p", error.message),
          workerAction("Copy CLI command", "copy", () => copy("rworker ps " + row.name)));
        destination.replaceChildren(unavailable);
      } else destination.replaceChildren(element("p", error.message, "empty"));
    }
  } finally {
    if (id === detailRequest) detailLoading = false;
  }
}

async function copy(value) {
  try {
    await navigator.clipboard.writeText(value);
    notice("Copied to clipboard.");
  } catch {
    notice("Could not access the clipboard.", true);
  }
}

const formTitles = {
  create: ["New worker", "Create worker"], extend: ["Extend lease", "Extend"],
  topic: ["Edit topic", "Save topic"], claim: ["Claim worker", "Claim worker"],
  delete: ["Delete worker", "Delete worker"], start: ["Start job", "Start job"],
  register: ["Register log", "Register log"], "renew-token": ["Renew cache token", "Renew token"],
};

async function form(action, worker, profile) {
  currentAction = { action, ...(worker ? { worker: worker.name } : {}) };
  const [title, submit] = formTitles[action];
  $("dialog-title").textContent = title;
  $("confirm").textContent = submit;
  $("confirm").classList.toggle("danger", action === "delete");
  $("form-error").hidden = true;
  $("form-fields").replaceChildren();
  const fields = $("form-fields");
  const field = (label, name, initial = "", kind = "input", choices = [], parent = fields) => {
    const wrap = element("div", null, "form-field");
    const node = element(kind);
    node.id = "field-" + name;
    node.name = name;
    if (kind === "select") {
      node.classList.add("profile-select");
      for (const choice of choices) {
        const option = element("option", profileGlyph(choice) + " " + choice.name);
        option.value = choice.name;
        option.setAttribute("aria-label", choice.name);
        node.append(option);
      }
    }
    node.value = initial;
    node.required = name !== "topic";
    if (name === "topic") node.maxLength = 255;
    const caption = element("label", label);
    caption.htmlFor = node.id;
    wrap.append(caption, node);
    parent.append(wrap);
    return node;
  };
  let profileInput;
  if (action === "create") {
    let profiles = state.rows.profiles;
    if (!profiles) {
      try { profiles = await request("profiles"); state.rows.profiles = profiles; }
      catch { profiles = []; }
    }
    if (!state.authenticated) return;
    const initial = profile || profiles.find((item) => item.default)?.name || profiles[0]?.name || "standard";
    profileInput = profiles.length ? field("Profile", "profile", initial, "select", profiles) :
      field("Profile", "profile", initial);
    field("Lifetime", "ttl", "2h");
  }
  if (action === "extend") field("Lifetime", "ttl", "2h");
  if (["create", "topic", "claim"].includes(action)) field("Topic", "topic", worker?.topic || "");
  if (profileInput) {
    const options = element("details", null, "create-options");
    options.append(element("summary", "More options"));
    const overrides = element("fieldset");
    overrides.append(element("legend", "OpenStack"));
    const flavor = field("Flavor override", "flavor", "", "input", [], overrides);
    const disk = field("Disk override (GiB)", "disk", "", "input", [], overrides);
    disk.type = "number";
    disk.min = "1";
    disk.max = "4294967295";
    disk.step = "1";
    const agent = field("Agent ID", "agent_id", "", "input", [], overrides);
    agent.maxLength = 128;
    agent.pattern = "[A-Za-z0-9._:][A-Za-z0-9._:\\-]*";
    for (const node of [flavor, disk, agent]) node.required = false;
    const toggle = (title, name) => {
      const label = checkbox(title, false, () => {});
      label.querySelector("input").name = name;
      return label;
    };
    overrides.append(toggle("Scratch worker (up to 7 days)", "scratch"),
      toggle("Skip niks3 push token", "no_niks3_token"));
    options.append(overrides, toggle("Open SSH after creation", "open_ssh"));
    fields.append(options);
    const updateProvider = () => {
      const profile = state.rows.profiles?.find((row) => row.name === profileInput.value);
      const ci = Object.hasOwn(ciProviders, profile?.provider) || /^(gh-(aarch64|x86_64)|gl-x86_64(-lease)?)$/.test(profileInput.value);
      overrides.hidden = overrides.disabled = ci;
      flavor.placeholder = ci ? "" : profile?.flavor || "Use profile default";
      disk.placeholder = ci || !profile?.disk ? "Use profile default" : String(profile.disk);
    };
    profileInput.addEventListener("input", updateProvider);
    updateProvider();
  }
  if (["start", "register"].includes(action)) field("Job name", "job");
  if (action === "start") field("Command", "command", "", "textarea");
  if (action === "register") field("Absolute log path on the worker", "path");
  if (action === "delete") fields.append(element("p", "Delete " + worker.name + " and release its compute resources?"));
  if (action === "renew-token") fields.append(element("p", "Renew the cache token for " + worker.name + "?"));
  $("dialog").showModal();
}

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
    $("console").hidden = false;
    $("lock").hidden = !token;
    if (!token && session?.signOutUrl?.startsWith("https://")) {
      $("sign-out").href = session.signOutUrl;
      $("sign-out").hidden = false;
    }
    notice("");
    navigate();
    followInventory();
  } catch (error) {
    lock();
    if (!automatic || error.message !== "Enter the console access token.") notice(error.message, true);
  }
}

$("unlock").addEventListener("submit", (event) => {
  event.preventDefault();
  const token = $("token").value.trim();
  $("token").value = "";
  if (token) openConsole(token);
});
$("lock").addEventListener("click", () => { lock(); notice(""); });
$("create").addEventListener("click", () => form("create"));
$("profile-close").addEventListener("click", () => $("profile-dialog").close());
$("search").addEventListener("input", () => { state.limit = 200; render(); });
$("extra").addEventListener("change", () => {
  state.showFinished = $("extra").checked;
  state.limit = 200;
  render();
});
for (const control of filterButtons) {
  control.addEventListener("click", () => {
    state.workerFilter = control.dataset.workerFilter;
    state.limit = 200;
    render();
  });
}
$("refresh").addEventListener("click", () => refresh(true));
$("cancel").addEventListener("click", () => $("dialog").close());
$("dismiss").addEventListener("click", () => $("dialog").close());
$("action-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const action = { ...currentAction, ...Object.fromEntries(new FormData(event.target)) };
  const openSsh = action.open_ssh === "on";
  if (action.action === "create") {
    delete action.open_ssh;
    for (const key of ["flavor", "disk", "agent_id"]) {
      if (!action[key]?.trim()) delete action[key];
    }
    if (action.disk) action.disk = Number(action.disk);
    for (const key of ["scratch", "no_niks3_token"]) action[key] = action[key] === "on";
  }
  $("confirm").disabled = true;
  $("form-error").hidden = true;
  try {
    const result = await request("action", action);
    $("dialog").close();
    notice(action.action === "create" ? "Worker is ready." : "Action completed.");
    if (action.action === "delete") location.hash = "workers";
    if (openSsh && result?.worker) openTerminal({ name: result.worker });
    await refresh(true);
    if (state.selected && !logSocket && action.action !== "delete") updateDetail();
  } catch (error) {
    $("form-error").textContent = error.message;
    $("form-error").hidden = false;
    if (!$("dialog").open) notice(error.message, true);
  } finally {
    $("confirm").disabled = false;
  }
});
window.addEventListener("hashchange", navigate);
document.addEventListener("click", (event) => {
  const menu = document.querySelector(".worker-action-menu[open]");
  if (menu && !menu.contains(event.target)) menu.open = false;
});
setInterval(() => { if (state.authenticated && !document.hidden && state.view !== "profiles") refresh(); }, 5000);
setInterval(() => { if (state.authenticated && !document.hidden && state.selected && state.tab === "logs" && state.follow && !logSocket) updateDetail(); }, 2500);
navigate();
let savedToken = "";
try { savedToken = sessionStorage.getItem("rworker.operator") || ""; } catch { /* Storage disabled. */ }
openConsole(savedToken, true);
