"use strict";

const $ = (id) => document.getElementById(id);
const views = {
  workers: ["Workers", "Manage worker leases and inspect build output."],
  jobs: ["Jobs", "Follow registered jobs across your workers."],
  events: ["Events", "Review the controller’s lifecycle history."],
  profiles: ["Profiles", "See the resources available for new workers."],
};
const state = {
  token: "", view: "workers", rows: {}, selected: null, job: null, tab: "logs",
  showRetired: false, showFinished: false, redact: true, follow: true, limit: 200,
};
let stream;
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
      Authorization: "Bearer " + access,
      ...(data ? { "Content-Type": "application/json" } : {}),
    },
    body: data ? JSON.stringify(data) : undefined,
    cache: "no-store",
  });
  if (response.status === 401) {
    if (state.token === access) lock();
    throw new Error("Enter the console access token.");
  }
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || "The controller did not respond.");
  return payload.data;
}

function lock() {
  state.token = "";
  state.rows = {};
  state.selected = null;
  detailRequest++;
  stream?.close();
  stream = undefined;
  $("console").hidden = true;
  $("login").hidden = false;
  $("lock").hidden = true;
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
  $("create").hidden = !state.token;
  const isWorkers = state.view === "workers";
  $("extra-toggle").hidden = !isWorkers && state.view !== "jobs";
  $("extra-label").textContent = isWorkers ? "Show retired" : "Show finished";
  $("extra").checked = isWorkers ? state.showRetired : state.showFinished;
  detailPanel();
  render();
  refresh();
}

function badge(value) {
  const text = String(value || "Unknown");
  const key = text.toLowerCase();
  const kind = /^(active|ready|exit 0)$/.test(key) ? "good" :
    /^(build|boot|running|reboot|deleting|pending)/.test(key) ? "busy" :
    /^(error|failed|exit [1-9])/.test(key) ? "bad" : "";
  return element("span", text, "badge" + (kind ? " " + kind : ""));
}

function ago(value) {
  if (!value) return "—";
  const date = typeof value === "number" ? value * 1000 : Date.parse(value);
  if (!Number.isFinite(date)) return "—";
  const minutes = Math.floor((Date.now() - date) / 60000);
  if (minutes < 1) return "Just now";
  if (minutes < 60) return minutes + "m ago";
  if (minutes < 1440) return Math.floor(minutes / 60) + "h ago";
  return Math.floor(minutes / 1440) + "d ago";
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
    visible = visible.filter((row) => !String(row.state).startsWith("exit "));
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
    case "workers":
      columns = [["Worker"], ["Status"], ["Agent", true], ["Age", true]];
      rows = visible.map((row) => ({
        selected: state.selected?.name === row.name,
        cells: [named(row.name, row.topic || row.profile, () => selected(row)), badge(row.retired ? "Retired" : row.status),
          row.agent || "—", ago(row.created_at)],
      }));
      break;
    case "jobs":
      columns = [["Job"], ["State"], ["Started", true]];
      rows = visible.map((row) => ({
        selected: state.selected?.name === row.worker && state.job === row.job,
        cells: [named(row.job, row.worker, () => selected(workerFor(row.worker), row.job)), badge(row.state), ago(row.started_at)],
      }));
      break;
    case "events":
      columns = [["Event"], ["Target"], ["Time", true]];
      rows = visible.map((row) => ({
        cells: [named(row.kind.replaceAll("-", " "), row.details, null), row.target, clock(row.timestamp)],
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
  if (!state.token || loading) return;
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
    connection("Connected", "online");
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
    if (state.token) connection("Disconnected", "error");
    if (manual || !state.rows[view]) notice(error.message, true);
  } finally {
    loading = false;
    if (state.token && state.view !== view) refresh();
  }
}

function detailPanel() {
  const row = state.selected;
  $("detail").hidden = !row;
  $("workspace").classList.toggle("has-detail", Boolean(row));
  if (!row) {
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
    datum.append(element("dt", key), element("dd", value));
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
    if (row.retired && !["logs", "inspect"].includes(key)) continue;
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

async function updateDetail(reset = false) {
  const row = state.selected;
  if (!row || !state.token || (detailLoading && !reset)) return;
  detailLoading = true;
  const tab = state.tab;
  const target = encodeURIComponent(row.name);
  const job = state.job;
  const id = ++detailRequest;
  const destination = $("detail-output");
  if (reset) destination.replaceChildren(element("p", "Loading…", "empty"));
  try {
    let data;
    if (tab === "logs") {
      const endpoint = row.retired ? (state.redact ? "archived-logs" : "raw-archive") : (state.redact ? "logs" : "raw-logs");
      data = await request(endpoint + "/" + target + (job ? "/" + encodeURIComponent(job) : ""));
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
    if (tab === "logs") {
      options.append(checkbox("Redact secrets", state.redact, (enabled) => { state.redact = enabled; updateDetail(true); }));
      if (!row.retired) options.append(checkbox("Follow", state.follow, (enabled) => { state.follow = enabled; if (enabled) updateDetail(); }));
      if (job) options.append(button("All jobs", () => { state.job = null; updateDetail(true); }));
    } else if (tab === "inspect" && !row.retired) {
      options.append(button("Load boot metadata", async () => {
        try {
          const metadata = await request("boot/" + target);
          if (id === detailRequest) destination.querySelector("pre").textContent = JSON.stringify(metadata, null, 2);
        } catch (error) { notice(error.message, true); }
      }));
    }
    const stripAnsi = (value) => value.replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "");
    const content = element("pre", typeof data === "string" ? (stripAnsi(data) || "No output yet.") : JSON.stringify(data, null, 2));
    destination.replaceChildren(options, content);
    if (state.follow && stayAtEnd && tab === "logs") content.scrollTop = content.scrollHeight;
    else content.scrollTop = previousScroll;
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
    if (!state.token) return;
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

$("unlock").addEventListener("submit", async (event) => {
  event.preventDefault();
  state.token = $("token").value.trim();
  $("token").value = "";
  if (!state.token) return;
  notice("");
  try {
    state.rows.workers = await request("workers");
    $("login").hidden = true;
    $("console").hidden = false;
    $("lock").hidden = false;
    navigate();
    stream = new EventSource("/v1/ui/updates");
    stream.onmessage = () => { if (state.view === "workers") refresh(); };
  } catch (error) {
    lock();
    notice(error.message, true);
  }
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
setInterval(() => { if (state.token && !document.hidden) refresh(); }, 5000);
setInterval(() => { if (state.token && !document.hidden && state.selected && state.tab === "logs" && state.follow) updateDetail(); }, 2500);
navigate();
