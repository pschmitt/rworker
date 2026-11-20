"use strict";

const $ = (id) => document.getElementById(id);
const views = {
  workers: ["Workers", "Manage worker leases and inspect build output."],
  jobs: ["Jobs", "Follow registered jobs across your workers."],
  events: ["Events", "Review the controller’s lifecycle history."],
  profiles: ["Profiles", "See the resources available for new workers."],
};
const state = {
  token: "", authenticated: false, view: "workers", rows: {}, selected: null, job: null, tab: "logs",
  showRetired: false, showFinished: false, redact: true, follow: true, limit: 200,
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
  $("connection").textContent = label;
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
  const requested = location.hash.slice(1);
  state.view = Object.hasOwn(views, requested) ? requested : "workers";
  state.selected = null;
  state.limit = 200;
  $("search").value = "";
  $("heading").textContent = views[state.view][0];
  $("subtitle").textContent = views[state.view][1];
  document.title = views[state.view][0] + " · rworker";
  document.querySelectorAll("[data-view]").forEach((link) => {
    if (link.dataset.view === state.view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  $("search").placeholder = "Filter " + state.view;
  $("create").hidden = !state.authenticated;
  const isWorkers = state.view === "workers";
  $("extra-toggle").hidden = !isWorkers && state.view !== "jobs";
  $("extra-label").textContent = isWorkers ? "Show retired" : "Show finished";
  $("extra").checked = isWorkers ? state.showRetired : state.showFinished;
  detailPanel();
  render();
  refresh();
}

const agentIcons = new Map([
  ["claude", ["\uec82", "claude"]], ["codex", ["\uec81", "codex"]],
  ["shell", ["\uea85", ""]], ["nixbot", ["\uf313", "nix"]], ["nix build", ["\uf313", "nix"]],
  ["copilot", ["\uec1e", ""]], ["gemini", ["\uec10", ""]], ["agy", ["\uec10", ""]],
  ["antigravity", ["\uec10", ""]], ["browser-daemon", ["\u{f02a0}", ""]],
]);

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

function workerAge(created, now) {
  const seconds = typeof created === "number" ? created : Date.parse(created) / 1000;
  return Number.isFinite(seconds) ? duration(now - seconds) : "—";
}

function workerExpiry(row, now) {
  let label = "—";
  let tone = "";
  if (row.kind === "pool" && row.pool_state) {
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
    else if (remaining < 900) tone = " soon";
  }
  return element("span", label, "worker-expiry" + tone);
}

function clock(value) {
  if (!value) return "—";
  const date = new Date(typeof value === "number" ? value * 1000 : value);
  return Number.isNaN(date.valueOf()) ? "—" : date.toLocaleString();
}

function table(columns, rows) {
  const node = element("table");
  const head = node.createTHead().insertRow();
  for (const [label, optional] of columns) {
    const cell = element("th", label, optional ? "optional" : "");
    cell.scope = "col";
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

function selected(row, job = null) {
  state.selected = row;
  state.job = job;
  state.tab = "logs";
  state.follow = !row.retired;
  detailPanel();
  render();
  updateDetail(true);
  if (window.matchMedia("(max-width: 1100px)").matches) {
    $("detail").scrollIntoView({ block: "start" });
  }
}

function sourceWorker(name) {
  return (state.rows.workers || []).find((worker) => worker.name === name);
}
function workerFor(name) {
  return sourceWorker(name) || { name };
}

function render() {
  const source = state.rows[state.view] || [];
  const results = $("results");
  const search = $("search").value.trim().toLowerCase();
  let visible = source.filter((row) => !search || JSON.stringify(row).toLowerCase().includes(search));
  const summary = $("summary");
  summary.replaceChildren();
  if (state.view === "workers") {
    const live = source.filter((row) => !row.retired);
    const ready = live.filter((row) => ["ACTIVE", "READY"].includes(row.status)).length;
    for (const [label, value] of [["Live workers", live.length], ["Ready", ready], ["Retired", source.length - live.length]]) {
      const cell = element("div");
      cell.append(element("strong", value), element("small", label));
      summary.append(cell);
    }
    if (!state.showRetired) visible = visible.filter((row) => !row.retired);
  }
  if (state.view === "jobs" && !state.showFinished) {
    visible = visible.filter((row) => row.state !== "archived" && !String(row.state).startsWith("exit "));
  }
  if (state.view === "events") visible = [...visible].reverse();
  const count = visible.length;
  visible = visible.slice(0, state.limit);

  if (!visible.length) {
    const message = !state.rows[state.view] ? "Loading " + state.view + "…" :
      search ? "No results match your filter." :
        state.view === "jobs" && !state.showFinished && source.length ? "No active jobs. Show finished jobs to see earlier runs." :
          "No " + state.view + " to display.";
    results.replaceChildren(element("div", message, "empty"));
    return;
  }
  let columns;
  let rows;
  switch (state.view) {
    case "workers": {
      const now = Date.now() / 1000;
      columns = [["Name"], ["Status"], ["Agent", true], ["Topic", true], ["Expires"], ["Age", true]];
      rows = visible.map((row) => {
        const name = named(row.name, row.topic, () => selected(row));
        name.className = "worker-name";
        return {
          selected: state.selected?.name === row.name,
          cells: [name, badge(row.retired ? "Retired" : row.status), agentLabel(row.agent),
            row.topic || "—", workerExpiry(row, now), element("span", workerAge(row.created_at, now), "worker-age")],
        };
      });
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
      columns = [["Event"], ["Target"], ["Time", true]];
      rows = visible.map((row) => ({
        cells: [eventLabel(row), element("span", row.target, "event-tone info"), clock(row.timestamp)],
      }));
      break;
    default:
      columns = [["Profile"], ["Flavor"], ["Disk"], ["Image tags", true]];
      rows = visible.map((row) => ({
        cells: [named(row.name, row.default ? "Default" : "", () => form("create", null, row.name)),
          row.flavor, row.disk ? row.disk + " GiB" : "—", (row.image_tags || []).join(", ") || "—"],
      }));
  }
  results.replaceChildren(table(columns, rows));
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
    connection(state.inventoryStale && view === "workers" ? "Cached inventory" : "Connected", "online");
    if (view === state.view) {
      if (state.selected && view === "workers") {
        const current = sourceWorker(state.selected.name);
        if (current && ["status", "owner", "retired", "expires_at"].some((field) => current[field] !== state.selected[field])) {
          state.selected = current;
          detailPanel();
          updateDetail(true);
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

function detailPanel() {
  const row = state.selected;
  $("detail").hidden = !row;
  $("workspace").classList.toggle("has-detail", Boolean(row));
  if (!row) {
    closeLogStream();
    detailRequest++;
    detailLoading = false;
    $("detail-output").replaceChildren();
    return;
  }
  $("detail-name").textContent = row.name;
  $("detail-state").replaceChildren(badge(row.retired ? "Retired" : row.status));
  $("metadata").replaceChildren();
  for (const [key, value] of [["Owner", row.owner], ["Profile", row.profile], ["Agent", row.agent], ["Expires", clock(row.expires_at)]]) {
    if (!value) continue;
    const datum = element("div");
    const description = element("dd", key === "Agent" ? null : value);
    if (key === "Agent") description.append(agentLabel(value));
    datum.append(element("dt", key), description);
    $("metadata").append(datum);
  }

  const actions = $("worker-actions");
  actions.replaceChildren();
  if (!row.retired) {
    actions.append(button("Start job", () => form("start", row)));
    const menu = element("details");
    menu.append(element("summary", "Actions"));
    for (const [label, action] of [["Extend", "extend"], ["Edit topic", "topic"], ["Claim", "claim"],
      ["Register log", "register"], ["Renew token", "renew-token"], ["Delete", "delete"]]) {
      menu.append(button(label, () => { menu.open = false; form(action, row); }));
    }
    menu.append(button("Copy SSH command", () => copy("rworker ssh " + row.name)),
      button("Copy builder entry", async () => {
        try { await copy(await request("builder/" + encodeURIComponent(row.name))); }
        catch (error) { notice(error.message, true); }
      }));
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
    logRetryAt = Date.now() + 15000;
    indicator.textContent = "Snapshot";
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
      const useArchive = row.retired || state.follow;
      const endpoint = useArchive ? (state.redact ? "archived-logs" : "raw-archive") : (state.redact ? "logs" : "raw-logs");
      data = await request(endpoint + "/" + target + (job ? "/" + encodeURIComponent(job) : "")).catch((error) => {
        if (row.retired || !state.follow || !state.authenticated) throw error;
        return "";
      });
    } else if (row.retired && tab === "inspect") {
      data = row;
    } else {
      data = await request(tab + "/" + target);
    }
    if (id !== detailRequest) return;
    if (tab === "jobs") {
      destination.replaceChildren(data.length ? table([["Job"], ["State"]], data.map((entry) => ({
        cells: [named(entry.job, "", () => { state.job = entry.job; state.tab = "logs"; detailPanel(); updateDetail(true); }), badge(entry.state)],
      }))) : element("p", "No jobs on this worker.", "empty"));
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
      if (job) options.append(button("All jobs", () => { state.job = null; updateDetail(true); }));
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
    if (id === detailRequest) destination.replaceChildren(element("p", error.message, "empty"));
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
  const field = (label, name, initial = "", kind = "input", choices = []) => {
    const wrap = element("div", null, "form-field");
    const node = element(kind);
    node.id = "field-" + name;
    node.name = name;
    if (kind === "select") {
      for (const choice of choices) {
        const option = element("option", choice);
        option.value = choice;
        node.append(option);
      }
    }
    node.value = initial;
    node.required = name !== "topic";
    if (name === "topic") node.maxLength = 255;
    const caption = element("label", label);
    caption.htmlFor = node.id;
    wrap.append(caption, node);
    fields.append(wrap);
  };
  if (action === "create") {
    let profiles = state.rows.profiles;
    if (!profiles) {
      try { profiles = await request("profiles"); state.rows.profiles = profiles; }
      catch { profiles = []; }
    }
    if (!state.authenticated) return;
    const names = profiles.map((item) => item.name);
    const initial = profile || profiles.find((item) => item.default)?.name || names[0] || "standard";
    if (names.length) field("Profile", "profile", initial, "select", names);
    else field("Profile", "profile", initial);
    field("Lifetime", "ttl", "2h");
  }
  if (action === "extend") field("Lifetime", "ttl", "2h");
  if (["create", "topic", "claim"].includes(action)) field("Topic", "topic", worker?.topic || "");
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
$("search").addEventListener("input", () => { state.limit = 200; render(); });
$("extra").addEventListener("change", () => {
  if (state.view === "workers") state.showRetired = $("extra").checked;
  if (state.view === "jobs") state.showFinished = $("extra").checked;
  render();
});
$("refresh").addEventListener("click", () => refresh(true));
$("close-detail").addEventListener("click", () => { state.selected = null; detailPanel(); render(); });
$("cancel").addEventListener("click", () => $("dialog").close());
$("dismiss").addEventListener("click", () => $("dialog").close());
$("action-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const action = { ...currentAction, ...Object.fromEntries(new FormData(event.target)) };
  $("confirm").disabled = true;
  $("form-error").hidden = true;
  try {
    await request("action", action);
    $("dialog").close();
    notice(action.action === "create" ? "Worker is ready." : "Action completed.");
    if (action.action === "delete") {
      state.selected = null;
      detailPanel();
    }
    await refresh(true);
    if (state.selected) { detailPanel(); updateDetail(); }
  } catch (error) {
    $("form-error").textContent = error.message;
    $("form-error").hidden = false;
    if (!$("dialog").open) notice(error.message, true);
  } finally {
    $("confirm").disabled = false;
  }
});
window.addEventListener("hashchange", navigate);
setInterval(() => { if (state.authenticated && !document.hidden) refresh(); }, 5000);
setInterval(() => { if (state.authenticated && !document.hidden && state.selected && state.tab === "logs" && state.follow && !logSocket) updateDetail(); }, 2500);
navigate();
let savedToken = "";
try { savedToken = sessionStorage.getItem("rworker.operator") || ""; } catch { /* Storage disabled. */ }
openConsole(savedToken, true);
