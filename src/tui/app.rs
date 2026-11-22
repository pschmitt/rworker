//! The TUI's state and keys. The worker list is the base view; jobs and
//! text views (logs, events, inspect, ps, command output) stack on it and
//! Esc goes back.

use super::View;
use super::create::{self, After, CreateWorker};
use super::proc::{self, Msg, Stream};
use crate::ctx::Ctx;
use crate::jq::{self, get};
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Direction, Position, Rect};
use ratatui::widgets::TableState;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

struct MouseCapture;

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
}

/// Lines kept per text view.
const MAX_LINES: usize = 20_000;

pub struct Worker {
    pub name: String,
    pub kind: String,
    pub profile: String,
    pub status: String,
    pub owner: String,
    pub agent: String,
    agent_id: String,
    pub topic: String,
    pub ips: String,
    pub created: Option<i64>,
    pub mine: bool,
    pub retired: bool,
    /// Nerd Font glyph of the worker's image (or its kind's fallback).
    pub glyph: String,
    raw: Value,
}

/// How soon a worker goes away, for the EXPIRES color.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    None,
    Soon,
    Now,
    Idle,
}

impl Worker {
    fn from(v: &Value, owner: &str) -> Worker {
        let s = |k: &str| jq::or_empty(get(v, k));
        let owner_v = s("owner");
        let kind = s("kind");
        Worker {
            name: s("name"),
            profile: match s("profile") {
                p if p.is_empty() => "standard".into(),
                p => p,
            },
            status: s("status").to_ascii_uppercase(),
            mine: kind == "adhoc" && owner_v == owner,
            kind,
            owner: owner_v,
            agent: s("agent"),
            agent_id: s("agent_id"),
            topic: s("topic"),
            ips: jq::or_empty(&crate::table::ips_label(v)),
            created: get(v, "created_at")
                .as_str()
                .and_then(crate::time::created_epoch),
            retired: false,
            glyph: crate::pool_icon::glyph(v).to_owned(),
            raw: v.clone(),
        }
    }

    /// EXPIRES of `ls`: the time left, the grace period, or a pool
    /// worker's state.
    pub fn expiry(&self, now: f64) -> (String, Urgency) {
        let w = &self.raw;
        let pool_state = get(w, "pool_state");
        if crate::table::is_pool(w) && jq::truthy(pool_state) {
            let last_busy = get(w, "last_busy");
            if pool_state.as_str() == Some("idle") && jq::truthy(last_busy) {
                let idle = crate::table::dur(now - jq::as_f64(last_busy));
                (format!("idle {idle}"), Urgency::Idle)
            } else {
                (jq::raw(pool_state), Urgency::None)
            }
        } else if jq::gt_num(get(w, "grace_until"), now) {
            let left = jq::as_f64(get(w, "grace_until")) - now;
            (format!("grace {}", crate::table::dur(left)), Urgency::Now)
        } else if jq::gt_num(get(w, "expires_at"), 0.0) {
            let left = jq::as_f64(get(w, "expires_at")) - now;
            let u = if left < 0.0 {
                Urgency::Now
            } else if left < 900.0 {
                Urgency::Soon
            } else {
                Urgency::None
            };
            (crate::table::dur(left), u)
        } else {
            ("-".into(), Urgency::None)
        }
    }

    pub fn age(&self, now: f64) -> String {
        self.created
            .map_or("-".into(), |c| crate::table::dur(now - c as f64))
    }

    fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || [
                &self.name,
                &self.kind,
                &self.status,
                &self.owner,
                &self.agent,
                &self.agent_id,
                &self.topic,
                &self.profile,
            ]
            .iter()
            .any(|f| f.to_lowercase().contains(needle))
    }
}

pub struct Jobs {
    pub worker: Option<String>,
    pub open_single: bool,
    pub show_finished: bool,
    pub rows: Vec<[String; 4]>,
    pub state: TableState,
    pub loading: Option<u64>,
    pub error: Option<String>,
}

pub struct Profiles {
    pub rows: Vec<Value>,
    pub state: TableState,
    request: u64,
    pub error: Option<String>,
}

pub struct TextView {
    pub title: String,
    pub worker: Option<String>,
    pub id: u64,
    pub lines: VecDeque<(String, bool)>,
    pub stream: Option<Stream>,
    /// How the process ended, once it has.
    pub exit: Option<Option<i32>>,
    pub follow: bool,
    pub top: usize,
    pub wrap: bool,
    pub filter: String,
    pub no_jobs: bool,
    jobs_loading: bool,
    /// Rows of the last draw, for paging.
    pub height: usize,
    /// The `WORKER/` (or `WORKER/JOB `) prefix of job log lines, which
    /// says nothing in a view of one worker (or job): pattern and
    /// replacement.
    pub(super) trim: Option<(regex::Regex, &'static str)>,
    seeded: bool,
    has_live_output: bool,
}

impl TextView {
    fn scroll(&mut self, delta: isize) {
        if delta < 0 {
            self.follow = false;
        }
        self.top = self.top.saturating_add_signed(delta);
    }
}

pub enum Sub {
    Jobs(Jobs),
    Profiles(Profiles),
    Text(TextView),
}

#[derive(Clone)]
pub enum PromptKind {
    Filter,
    TextFilter,
    Extend(String),
    Topic(String),
    Claim(String),
    Start(String),
    Command,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub input: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub hint: String,
}

pub struct Confirm {
    pub text: String,
    pub args: Vec<String>,
    pub label: String,
}

/// What a background action is for.
enum Pending {
    Status {
        label: String,
        deleting: Option<String>,
    },
    JobList,
    ProfileList,
    History,
    Profiles,
    ProvisionSsh {
        remove: bool,
        stage: String,
        started: Instant,
    },
    LogPreview(u64),
    PreviewJobs(u64),
}

pub struct Flash {
    pub text: String,
    pub error: bool,
    at: Instant,
}

pub(super) struct LogDivider {
    pub area: Rect,
    pub border: Rect,
    pub direction: Direction,
}

pub struct App {
    pub control_host: String,
    pub owner: String,
    pub icons: bool,
    pub workers: Vec<Worker>,
    pub show_history: bool,
    history: Vec<Worker>,
    history_loading: bool,
    pub table: TableState,
    pub mouse_table: Option<Rect>,
    pub(super) mouse_profile_create: Option<Rect>,
    pub mouse_text: Option<Rect>,
    pub(super) log_divider: Option<LogDivider>,
    pub(super) log_drag: Option<Direction>,
    pub(super) log_width: Option<u16>,
    pub(super) log_height: Option<u16>,
    pub preview: Option<(String, TextView)>,
    pub filter: String,
    pub mine_only: bool,
    pub refreshing: bool,
    pub inventory_live: bool,
    pub refreshed: Option<(Instant, Duration)>,
    pub stale: bool,
    pub refresh_error: Option<String>,
    pub stack: Vec<Sub>,
    pub prompt: Option<Prompt>,
    pub(super) create_worker: Option<CreateWorker>,
    pub confirm: Option<Confirm>,
    pub help: bool,
    pub(super) help_top: u16,
    pub plain: bool,
    pub flash: Option<Flash>,
    pub tick: u64,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    tx: Sender<Msg>,
    refresh: Sender<()>,
    suspend: Option<Vec<String>>,
    ready_ssh: Option<Vec<String>>,
    quit: bool,
}

impl App {
    /// The workers shown, after the filter, sorted like `ls`.
    pub fn visible(&self) -> Vec<&Worker> {
        let needle = self.filter.to_lowercase();
        let live = self.show_history.then(|| {
            self.workers
                .iter()
                .map(|w| &w.name)
                .collect::<std::collections::HashSet<_>>()
        });
        self.workers
            .iter()
            .chain(self.history.iter().filter(|worker| {
                live.as_ref()
                    .is_some_and(|live| !live.contains(&worker.name))
            }))
            .filter(|w| (!self.mine_only || w.mine) && w.matches(&needle))
            .collect()
    }

    pub fn selected(&self) -> Option<&Worker> {
        self.visible().get(self.table.selected()?).copied()
    }

    pub fn pending_actions(&self) -> usize {
        self.pending
            .values()
            .filter(|p| matches!(p, Pending::Status { .. }))
            .count()
    }

    pub(super) fn provisioning_ssh(&self) -> Option<(&str, Duration)> {
        self.pending.values().find_map(|action| match action {
            Pending::ProvisionSsh { stage, started, .. } => {
                Some((stage.as_str(), started.elapsed()))
            }
            _ => None,
        })
    }

    pub fn flash_live(&self) -> Option<&Flash> {
        self.flash
            .as_ref()
            .filter(|f| f.at.elapsed() < Duration::from_secs(if f.error { 15 } else { 6 }))
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.flash = Some(Flash {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn set_workers(&mut self, list: &[Value]) {
        let keep = self.selected().map(|w| w.name.clone());
        let mut workers: Vec<Worker> = list.iter().map(|v| Worker::from(v, &self.owner)).collect();
        workers.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        self.workers = workers;
        self.reselect(keep);
    }

    /// Keep the selection on the same worker when the list changes.
    fn reselect(&mut self, name: Option<String>) {
        let visible = self.visible();
        let idx = name
            .and_then(|n| visible.iter().position(|w| w.name == n))
            .or_else(|| {
                (!visible.is_empty())
                    .then(|| self.table.selected().unwrap_or(0).min(visible.len() - 1))
            });
        self.table.select(idx);
    }

    /// Run `rworker ARGS` in the background; LABEL is shown when it ends.
    fn act(&mut self, args: Vec<String>, label: impl Into<String>) {
        let label = label.into();
        let id = self.id();
        self.say(format!("{label}…"), false);
        let deleting = if args.first().is_some_and(|arg| arg == "down") {
            args.get(1).cloned()
        } else {
            None
        };
        self.pending.insert(id, Pending::Status { label, deleting });
        proc::action(id, args, self.tx.clone());
    }

    pub(super) fn worker_status<'a>(&self, worker: &'a Worker) -> &'a str {
        if !worker.retired
            && !matches!(worker.status.as_str(), "DELETING" | "DELETED")
            && self.pending.values().any(|action| {
                matches!(action, Pending::Status { deleting: Some(name), .. } if name == &worker.name)
            })
        {
            "DOWN…"
        } else {
            &worker.status
        }
    }

    /// Open a text view on the output of `rworker ARGS`.
    fn open_text(&mut self, title: impl Into<String>, args: Vec<String>) {
        let view = self.text_stream(title, args);
        self.stack.push(Sub::Text(view));
    }

    fn text_stream(&mut self, title: impl Into<String>, args: Vec<String>) -> TextView {
        let id = self.id();
        let mut view = TextView {
            title: title.into(),
            worker: (args.first().is_some_and(|arg| arg == "logs"))
                .then(|| args.get(1).filter(|arg| !arg.starts_with('-')).cloned())
                .flatten(),
            id,
            lines: VecDeque::new(),
            stream: None,
            exit: None,
            follow: true,
            top: 0,
            wrap: false,
            filter: String::new(),
            no_jobs: false,
            jobs_loading: false,
            height: 20,
            trim: log_prefix(&args),
            seeded: false,
            has_live_output: false,
        };
        match Stream::spawn(id, &args, &self.tx) {
            Ok(s) => view.stream = Some(s),
            Err(e) => {
                view.lines
                    .push_back((format!("could not start: {e}"), true));
                view.exit = Some(None);
            }
        }
        view
    }

    fn open_view(&mut self, view: View) {
        match view {
            View::Workers => {}
            View::Jobs => self.open_jobs(None),
            View::Profiles => {
                let id = self.id();
                self.stack.push(Sub::Profiles(Profiles {
                    rows: Vec::new(),
                    state: TableState::default(),
                    request: id,
                    error: None,
                }));
                self.pending.insert(id, Pending::ProfileList);
                proc::action(id, s(&["profiles", "--json"]), self.tx.clone());
            }
            View::Events => self.open_text("events", s(&["events", "--watch"])),
            View::Logs => self.open_text("logs · all workers", s(&["logs", "--follow"])),
        }
    }

    fn open_jobs(&mut self, worker: Option<String>) {
        self.stack.push(Sub::Jobs(Jobs {
            worker,
            open_single: false,
            show_finished: false,
            rows: Vec::new(),
            state: TableState::default(),
            loading: None,
            error: None,
        }));
        self.reload_jobs();
    }

    fn toggle_history(&mut self) {
        let keep = self.selected().map(|w| w.name.clone());
        self.show_history = !self.show_history;
        self.reselect(keep);
        if self.show_history {
            self.load_history();
        }
    }

    fn load_history(&mut self) {
        if self.history_loading {
            return;
        }
        self.history_loading = true;
        let id = self.id();
        self.pending.insert(id, Pending::History);
        proc::action(id, s(&["__worker-history"]), self.tx.clone());
    }

    fn reload_jobs(&mut self) {
        let id = self.id();
        if let Some(Sub::Jobs(j)) = self.stack.last_mut()
            && j.loading.is_none()
        {
            j.loading = Some(id);
            let args = std::iter::once("jobs".to_owned())
                .chain(j.worker.clone())
                .collect();
            self.pending.insert(id, Pending::JobList);
            proc::action(id, args, self.tx.clone());
        }
    }

    fn ask(&mut self, kind: PromptKind, title: &str, input: &str, hint: &str) {
        self.prompt = Some(Prompt {
            kind,
            title: title.into(),
            cursor: input.chars().count(),
            input: input.into(),
            hint: hint.into(),
        });
    }

    fn sync_preview(&mut self) {
        let selected = if self.stack.is_empty() && self.mouse_text.is_some() {
            self.selected()
                .map(|worker| (worker.name.as_str(), worker.retired))
        } else {
            None
        };
        if self
            .preview
            .as_ref()
            .map(|(worker, view)| (worker.as_str(), view.title.starts_with("cached logs")))
            == selected
        {
            return;
        }
        let selected = selected.map(|(name, retired)| (name.to_owned(), retired));
        self.preview = None;
        if let Some((worker, retired)) = selected {
            let args = if retired {
                s(&["logs", &worker, "--cached", "--lines", "100"])
            } else {
                s(&["logs", &worker, "--follow", "--quiet", "--lines", "100"])
            };
            let label = if retired { "cached logs" } else { "logs" };
            let mut view = self.text_stream(format!("{label} · {worker}"), args);
            view.wrap = true;
            if !retired && std::env::var_os("RWORKER_LOG_CACHE_READ").is_some() {
                let id = self.id();
                self.pending.insert(id, Pending::LogPreview(view.id));
                proc::action(
                    id,
                    vec!["__cached-preview".into(), worker.clone()],
                    self.tx.clone(),
                );
            }
            self.preview = Some((worker, view));
            if !retired {
                self.refresh_preview_jobs();
            }
        }
    }

    fn refresh_preview_jobs(&mut self) {
        let Some((worker, view)) = &mut self.preview else {
            return;
        };
        if view.jobs_loading || view.title.starts_with("cached logs") {
            return;
        }
        view.jobs_loading = true;
        let worker = worker.clone();
        let view_id = view.id;
        let id = self.id();
        self.pending.insert(id, Pending::PreviewJobs(view_id));
        proc::action(id, vec!["jobs".into(), worker], self.tx.clone());
    }

    fn text_view(&mut self, id: u64) -> Option<&mut TextView> {
        self.stack
            .iter_mut()
            .filter_map(|sub| match sub {
                Sub::Text(view) => Some(view),
                Sub::Jobs(_) | Sub::Profiles(_) => None,
            })
            .chain(self.preview.as_mut().map(|(_, view)| view))
            .find(|view| view.id == id)
    }

    fn mouse(&mut self, event: MouseEvent) {
        if event.kind == MouseEventKind::Up(MouseButton::Left) {
            self.log_drag = None;
            return;
        }
        if self.help
            || self.prompt.is_some()
            || self.create_worker.is_some()
            || self.confirm.is_some()
        {
            return;
        }
        let point = Position::new(event.column, event.row);
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .mouse_profile_create
                .is_some_and(|area| area.contains(point))
        {
            self.create_from_selected_profile();
            return;
        }
        if let Some(divider) = &self.log_divider {
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && divider.border.contains(point)
            {
                self.log_drag = Some(divider.direction);
                return;
            }
            if event.kind == MouseEventKind::Drag(MouseButton::Left)
                && self.log_drag == Some(divider.direction)
            {
                match divider.direction {
                    Direction::Horizontal => {
                        self.log_width = Some(super::ui::log_size(
                            divider.area.width,
                            divider.area.right().saturating_sub(event.column),
                            divider.direction,
                        ));
                    }
                    Direction::Vertical => {
                        self.log_height = Some(super::ui::log_size(
                            divider.area.height,
                            divider.area.bottom().saturating_sub(event.row),
                            divider.direction,
                        ));
                    }
                }
                return;
            }
        }

        let delta = match event.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::Down(MouseButton::Left) => 0,
            _ => return,
        };
        if self.mouse_text.is_some_and(|area| area.contains(point)) {
            let view = match self.stack.last_mut() {
                Some(Sub::Text(view)) => Some(view),
                None => self.preview.as_mut().map(|(_, view)| view),
                Some(Sub::Jobs(_) | Sub::Profiles(_)) => None,
            };
            if delta != 0
                && let Some(view) = view
            {
                view.scroll(delta);
            }
        } else if let Some(area) = self.mouse_table.filter(|area| area.contains(point)) {
            let offset = usize::from(event.row - area.y);
            let worker_count = self.visible().len();
            let (state, count) = match self.stack.last_mut() {
                Some(Sub::Jobs(jobs)) => (&mut jobs.state, jobs.rows.len()),
                Some(Sub::Profiles(profiles)) => (&mut profiles.state, profiles.rows.len()),
                None => (&mut self.table, worker_count),
                Some(Sub::Text(_)) => return,
            };
            if count == 0 {
                return;
            }
            let index = if delta == 0 {
                state.offset() + offset
            } else {
                state
                    .selected()
                    .unwrap_or(0)
                    .saturating_add_signed(delta)
                    .min(count - 1)
            };
            if index < count {
                state.select(Some(index));
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            Msg::Input(Event::Mouse(event)) => self.mouse(event),
            Msg::Input(_) => {}
            Msg::Refreshing => self.refreshing = true,
            Msg::InventoryLive(live) => self.inventory_live = live,
            Msg::Workers { result, took } => {
                self.refreshing = false;
                match result {
                    Ok(list) => {
                        let removed = self.show_history
                            && self.workers.iter().any(|worker| {
                                !list
                                    .iter()
                                    .any(|row| row["name"].as_str() == Some(&worker.name))
                            });
                        self.set_workers(&list);
                        if removed {
                            self.load_history();
                        }
                        self.refreshed = Some((Instant::now(), took));
                        self.stale = false;
                        self.refresh_error = None;
                        if matches!(self.stack.last(), Some(Sub::Jobs(j)) if j.worker.is_none()) {
                            self.reload_jobs();
                        }
                    }
                    Err(e) => self.refresh_error = Some(e),
                }
            }
            Msg::Line { id, text, err } => {
                if let Some(v) = self.text_view(id) {
                    if !err {
                        if v.seeded {
                            v.lines.clear();
                            v.top = 0;
                            v.seeded = false;
                        }
                        v.has_live_output = true;
                    }
                    v.lines.push_back((text, err));
                    if v.lines.len() > MAX_LINES {
                        v.lines.pop_front();
                        v.top = v.top.saturating_sub(1);
                    }
                }
            }
            Msg::Exit { id } => {
                if let Some(v) = self.text_view(id) {
                    v.exit = Some(v.stream.as_mut().and_then(Stream::code));
                    v.stream = None;
                }
            }
            Msg::Progress { id, stage } => {
                if let Some(Pending::ProvisionSsh { stage: current, .. }) =
                    self.pending.get_mut(&id)
                {
                    *current = stage;
                }
            }
            Msg::Done { id, ok, out, err } => match self.pending.remove(&id) {
                Some(Pending::Status { label, .. }) => {
                    if ok {
                        let detail = proc::last_line(&out);
                        if detail.is_empty() || detail.starts_with('{') || detail.starts_with('[') {
                            self.say(format!("{label}: done"), false);
                        } else {
                            self.say(format!("{label}: {detail}"), false);
                        }
                    } else {
                        let why = proc::last_line(&err);
                        self.say(
                            format!(
                                "{label} failed: {}",
                                if why.is_empty() { "error" } else { &why }
                            ),
                            true,
                        );
                    }
                    let _ = self.refresh.send(());
                }
                Some(Pending::LogPreview(view_id)) => {
                    if ok
                        && let Some(view) = self.text_view(view_id)
                        && !view.has_live_output
                    {
                        view.lines.extend(
                            out.lines()
                                .rev()
                                .take(MAX_LINES)
                                .map(|text| (text.to_owned(), false)),
                        );
                        view.lines.make_contiguous().reverse();
                        view.seeded = true;
                    }
                }
                Some(Pending::PreviewJobs(view_id)) => {
                    if let Some(view) = self.text_view(view_id) {
                        view.jobs_loading = false;
                        view.no_jobs = ok && out.trim().is_empty();
                    }
                }
                Some(Pending::ProvisionSsh { remove, .. }) => {
                    let ready = serde_json::from_str::<Value>(&out)
                        .ok()
                        .filter(|_| ok)
                        .and_then(|info| {
                            Some((
                                info["name"].as_str()?.to_owned(),
                                info["user"].as_str()?.to_owned(),
                            ))
                        });
                    if let Some((name, user)) = ready {
                        let mut args = s(&["__connect-created", &name, &user]);
                        if remove {
                            args.push("--rm".into());
                        }
                        self.ready_ssh = Some(args);
                    } else {
                        let error = proc::last_line(&err);
                        self.say(
                            if error.is_empty() {
                                "Worker provisioning failed".to_owned()
                            } else {
                                error
                            },
                            true,
                        );
                    }
                    let _ = self.refresh.send(());
                }
                Some(Pending::Profiles) => {
                    if let Some(form) = self
                        .create_worker
                        .as_mut()
                        .filter(|form| form.request == id)
                    {
                        let result = if ok {
                            serde_json::from_str(&out).map_err(|error| error.to_string())
                        } else {
                            Err(err)
                        };
                        form.load(result);
                    }
                }
                Some(Pending::History) => {
                    self.history_loading = false;
                    let keep = self.selected().map(|w| w.name.clone());
                    match serde_json::from_str::<Vec<Value>>(&out) {
                        Ok(rows) if ok => {
                            self.history = rows
                                .iter()
                                .map(|row| {
                                    let mut worker = Worker::from(row, &self.owner);
                                    worker.retired = true;
                                    worker.mine = true;
                                    worker.status = row["exit_code"].as_i64().map_or_else(
                                        || "RETIRED".into(),
                                        |code| format!("EXIT {code}"),
                                    );
                                    worker
                                })
                                .collect();
                            self.reselect(keep);
                        }
                        _ if self.show_history => self.say("Could not load worker history", true),
                        _ => {}
                    }
                }
                Some(Pending::ProfileList) => {
                    if let Some(Sub::Profiles(profiles)) = self.stack.last_mut()
                        && profiles.request == id
                    {
                        match serde_json::from_str::<Vec<Value>>(&out) {
                            Ok(rows) if ok && !rows.is_empty() => {
                                profiles.state.select(Some(0));
                                profiles.rows = rows;
                            }
                            Ok(_) if ok => profiles.error = Some("No profiles available".into()),
                            _ => {
                                let reason = proc::last_line(&err);
                                profiles.error = Some(if reason.is_empty() {
                                    "Could not list profiles".into()
                                } else {
                                    reason
                                });
                            }
                        }
                    }
                }
                Some(Pending::JobList) => {
                    let mut single = None;
                    for sub in &mut self.stack {
                        if let Sub::Jobs(j) = sub
                            && j.loading == Some(id)
                        {
                            j.loading = None;
                            let why = proc::last_line(&err);
                            j.error = if why.is_empty() {
                                (!ok).then(|| "could not list jobs".into())
                            } else {
                                Some(why)
                            };
                            if ok {
                                let selected = j
                                    .state
                                    .selected()
                                    .and_then(|index| j.rows.get(index))
                                    .map(|row| (row[0].clone(), row[1].clone()));
                                j.rows = out
                                    .lines()
                                    .filter(|line| !line.is_empty())
                                    .filter_map(|line| {
                                        let mut fields = line
                                            .splitn(if j.worker.is_some() { 3 } else { 4 }, '\t');
                                        let worker =
                                            j.worker.as_deref().or_else(|| fields.next())?;
                                        let job = fields.next()?;
                                        let state = fields.next()?;
                                        if !j.show_finished
                                            && !matches!(state, "running" | "registered")
                                        {
                                            return None;
                                        }
                                        Some([
                                            worker.to_owned(),
                                            job.to_owned(),
                                            state.to_owned(),
                                            fields.next()?.to_owned(),
                                        ])
                                    })
                                    .collect();
                                let n = j.rows.len();
                                let sel = selected
                                    .and_then(|(worker, job)| {
                                        j.rows
                                            .iter()
                                            .position(|row| row[0] == worker && row[1] == job)
                                    })
                                    .unwrap_or_else(|| {
                                        j.state.selected().unwrap_or(0).min(n.saturating_sub(1))
                                    });
                                j.state.select((n > 0).then_some(sel));
                                if j.open_single && n == 1 {
                                    single = Some((j.rows[0][0].clone(), j.rows[0][1].clone()));
                                }
                                j.open_single = false;
                            }
                        }
                    }
                    if let Some((worker, job)) = single {
                        self.open_text(
                            format!("logs · {worker}/{job}"),
                            vec![
                                "logs".into(),
                                worker,
                                job,
                                "--follow".into(),
                                "--lines".into(),
                                "1000".into(),
                            ],
                        );
                    }
                }
                None => {}
            },
        }
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Legacy terminals encode Alt+Shift+Q as Escape followed by Q.
        let alt_shift_q = k.modifiers.contains(KeyModifiers::ALT)
            && !ctrl
            && (k.code == KeyCode::Char('Q')
                || (k.code == KeyCode::Char('q') && k.modifiers.contains(KeyModifiers::SHIFT)));
        if (ctrl && k.code == KeyCode::Char('c')) || alt_shift_q {
            self.quit = true;
            return;
        }
        if self.help {
            match k.code {
                KeyCode::Down => self.help_top = self.help_top.saturating_add(1),
                KeyCode::Up => self.help_top = self.help_top.saturating_sub(1),
                KeyCode::PageDown => self.help_top = self.help_top.saturating_add(10),
                KeyCode::PageUp => self.help_top = self.help_top.saturating_sub(10),
                _ => self.help = false,
            }
            return;
        }
        if k.code == KeyCode::F(1)
            || (k.code == KeyCode::Char('?')
                && self.prompt.is_none()
                && self
                    .create_worker
                    .as_ref()
                    .is_none_or(|form| form.input(form.focus).is_none()))
        {
            self.help = true;
            self.help_top = 0;
            return;
        }
        let ssh_busy = self.provisioning_ssh().is_some() || self.ready_ssh.is_some();
        if let Some(form) = &mut self.create_worker {
            match k.code {
                KeyCode::Esc => self.create_worker = None,
                KeyCode::Enter => match form.submission() {
                    Ok((args, After::Create)) => {
                        self.create_worker = None;
                        self.act(args, "new worker");
                    }
                    Ok((_, _)) if ssh_busy => {
                        form.error = Some("An SSH worker is already provisioning.".into());
                    }
                    Ok((mut args, after)) => {
                        self.create_worker = None;
                        let id = self.id();
                        let remove = after == After::SshAndDelete;
                        self.pending.insert(
                            id,
                            Pending::ProvisionSsh {
                                remove,
                                stage: "REQUESTING".into(),
                                started: Instant::now(),
                            },
                        );
                        args[0] = "__tui-provision-ssh".into();
                        if remove {
                            args.insert(1, "--rm".into());
                        }
                        proc::action(id, args, self.tx.clone());
                    }
                    Err(error) => form.error = Some(error),
                },
                _ => form.key(k),
            }
            return;
        }
        if self.prompt.is_some() {
            self.prompt_key(k);
            return;
        }
        if let Some(c) = self.confirm.take() {
            match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.act(c.args, c.label),
                _ => self.say("cancelled", false),
            }
            return;
        }
        match k.code {
            KeyCode::Char('n') if k.modifiers.is_empty() => {
                self.plain = !self.plain;
                return;
            }
            KeyCode::Char(':') => {
                self.ask(
                    PromptKind::Command,
                    "rworker",
                    "",
                    "e.g. sync NAME ~/src · ssh takes the terminal",
                );
                return;
            }
            _ => {}
        }
        match self.stack.last() {
            None => self.workers_key(k),
            Some(Sub::Jobs(_)) => self.jobs_key(k),
            Some(Sub::Profiles(_)) => self.profiles_key(k),
            Some(Sub::Text(_)) => self.text_key(k),
        }
    }

    fn workers_key(&mut self, k: KeyEvent) {
        let n = self.visible().len();
        let sel = self.table.selected().unwrap_or(0);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let selected = self.selected();
        let retired = selected.is_some_and(|worker| worker.retired);
        let name = selected.map(|worker| worker.name.clone());
        if retired && matches!(k.code, KeyCode::Enter | KeyCode::Char('l' | 'o')) {
            if let Some(name) = &name {
                self.open_text(
                    format!("cached logs · {name}"),
                    s(&["logs", name, "--cached", "--lines", "1000"]),
                );
            }
            return;
        }
        if retired
            && matches!(
                k.code,
                KeyCode::Delete
                    | KeyCode::Char('s' | 'i' | 'I' | 'p' | 'b' | 'y' | 'e' | 't' | 'K' | 'N')
            )
            || retired && k.code == KeyCode::Char('d') && !ctrl
        {
            self.say("Worker is retired. Cached logs are available.", false);
            return;
        }
        let need = |app: &mut App| -> Option<String> {
            if name.is_none() {
                app.say("no worker selected", true);
            }
            name.clone()
        };
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() || self.mine_only {
                    self.filter.clear();
                    self.mine_only = false;
                    self.reselect(name.clone());
                }
            }
            KeyCode::Char('J') => self.open_view(View::Jobs),
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.table.select(Some((sel + 1).min(n - 1)))
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.table.select(Some(sel.saturating_sub(1)))
            }
            KeyCode::Home | KeyCode::Char('g') if n > 0 => self.table.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') if n > 0 => self.table.select(Some(n - 1)),
            KeyCode::PageDown if n > 0 => self.table.select(Some((sel + 10).min(n - 1))),
            KeyCode::PageUp if n > 0 => self.table.select(Some(sel.saturating_sub(10))),
            KeyCode::Char('d') if ctrl && n > 0 => self.table.select(Some((sel + 10).min(n - 1))),
            KeyCode::Char('u') if ctrl && n > 0 => self.table.select(Some(sel.saturating_sub(10))),
            KeyCode::Char('/') => {
                let f = self.filter.clone();
                self.ask(
                    PromptKind::Filter,
                    "filter",
                    &f,
                    "name, topic, agent, owner, …",
                );
            }
            KeyCode::Char('m') => {
                self.mine_only = !self.mine_only;
                self.reselect(name.clone());
            }
            KeyCode::Char('r') => {
                if self.show_history {
                    self.load_history();
                }
                let _ = self.refresh.send(());
            }
            KeyCode::Char('L') => self.open_view(View::Logs),
            KeyCode::Char('E') => self.open_view(View::Events),
            KeyCode::Char('P') => self.open_view(View::Profiles),
            KeyCode::Char('c') => {
                let id = self.id();
                self.create_worker = Some(CreateWorker::new(id));
                self.pending.insert(id, Pending::Profiles);
                proc::action(id, s(&["profiles", "--json"]), self.tx.clone());
            }
            KeyCode::Char('X') => {
                let mine: Vec<String> = self
                    .workers
                    .iter()
                    .filter(|w| w.mine)
                    .map(|w| w.name.clone())
                    .collect();
                if mine.is_empty() {
                    self.say("none of the ad-hoc workers is yours", false);
                } else {
                    self.confirm = Some(Confirm {
                        text: format!(
                            "Delete all {} of your ad-hoc workers?\n{}",
                            mine.len(),
                            mine.join(" ")
                        ),
                        args: s(&["prune", "--yes"]),
                        label: format!("prune {} workers", mine.len()),
                    });
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(w) = need(self) {
                    self.open_jobs(Some(w));
                    if let Some(Sub::Jobs(j)) = self.stack.last_mut() {
                        j.open_single = true;
                    }
                }
            }
            KeyCode::Char('l') => {
                if let Some(w) = need(self) {
                    self.open_text(
                        format!("logs · {w}"),
                        vec!["logs".into(), w, "--follow".into()],
                    );
                }
            }
            KeyCode::Char('i') => {
                if let Some(w) = need(self) {
                    self.open_text(format!("inspect · {w}"), vec!["inspect".into(), w]);
                }
            }
            KeyCode::Char('I') => {
                if let Some(w) = need(self) {
                    self.open_text(
                        format!("inspect --boot · {w}"),
                        vec!["inspect".into(), w, "--boot".into()],
                    );
                }
            }
            KeyCode::Char('p') => {
                if let Some(w) = need(self) {
                    self.open_text(format!("ps · {w}"), vec!["ps".into(), w, "--long".into()]);
                }
            }
            KeyCode::Char('b') => {
                if let Some(w) = need(self) {
                    self.open_text(format!("builder · {w}"), vec!["builder".into(), w]);
                }
            }
            KeyCode::Char('s') => {
                if let Some(w) = need(self) {
                    self.suspend = Some(vec!["ssh".into(), w]);
                }
            }
            KeyCode::Char('y') => {
                if let Some(w) = need(self) {
                    copy(&w);
                    self.say(format!("copied {w}"), false);
                }
            }
            KeyCode::Char('e') => {
                if let Some(w) = need(self) {
                    self.ask(
                        PromptKind::Extend(w),
                        "extend: new lifetime from now",
                        "2h",
                        "e.g. 90m, 4h, 24h",
                    );
                }
            }
            KeyCode::Char('t') => {
                if let Some(w) = self.selected().map(|w| (w.name.clone(), w.topic.clone())) {
                    self.ask(PromptKind::Topic(w.0), "topic", &w.1, "empty clears it");
                }
            }
            KeyCode::Char('C') => {
                if let Some(w) = self.selected().map(|w| (w.name.clone(), w.topic.clone())) {
                    self.ask(
                        PromptKind::Claim(w.0),
                        "claim: new topic",
                        &w.1,
                        "take the worker over",
                    );
                }
            }
            KeyCode::Char('K') => {
                if let Some(w) = need(self) {
                    self.act(
                        vec!["niks3-token".into(), w.clone()],
                        format!("niks3 token for {w}"),
                    );
                }
            }
            KeyCode::Char('N') => {
                if let Some(w) = need(self) {
                    self.act(
                        vec!["nix-cache".into(), w.clone()],
                        format!("nix-cache {w}"),
                    );
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(w) = self.selected() {
                    let force = !w.mine;
                    let mut args = vec!["down".to_string(), w.name.clone()];
                    if force {
                        args.push("--force".into());
                    }
                    let whose = if crate::table::is_pool(&w.raw) {
                        format!(" This is a {} pool worker: its builds fail.", w.kind)
                    } else if force {
                        format!(
                            " It belongs to {}.",
                            if w.owner.is_empty() {
                                "someone else"
                            } else {
                                &w.owner
                            }
                        )
                    } else {
                        String::new()
                    };
                    self.confirm = Some(Confirm {
                        text: format!("Delete {}?{whose}", w.name),
                        label: format!("down {}", w.name),
                        args,
                    });
                }
            }
            _ => {}
        }
    }

    fn jobs_key(&mut self, k: KeyEvent) {
        let Some(Sub::Jobs(j)) = self.stack.last_mut() else {
            return;
        };
        let n = j.rows.len();
        let sel = j.state.selected().unwrap_or(0);
        let row = j.state.selected().and_then(|index| j.rows.get(index));
        let job = row.map(|row| row[1].clone());
        let worker = row.map(|row| row[0].clone()).or_else(|| j.worker.clone());
        match k.code {
            KeyCode::Char('J') => self.stack.clear(),
            KeyCode::Char('d') => {
                j.show_finished = !j.show_finished;
                if j.show_finished {
                    self.reload_jobs();
                } else {
                    j.rows
                        .retain(|row| matches!(row[2].as_str(), "running" | "registered"));
                    j.state.select((!j.rows.is_empty()).then_some(0));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Char('h') => {
                self.stack.pop();
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                j.state.select(Some((sel + 1).min(n - 1)))
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                j.state.select(Some(sel.saturating_sub(1)))
            }
            KeyCode::Char('g') if n > 0 => j.state.select(Some(0)),
            KeyCode::Char('G') if n > 0 => j.state.select(Some(n - 1)),
            KeyCode::Char('r') => self.reload_jobs(),
            KeyCode::Enter | KeyCode::Char('l') if worker.is_some() => {
                let worker = worker.unwrap_or_default();
                if let Some(job) = job.filter(|_| k.code == KeyCode::Enter) {
                    self.open_text(
                        format!("logs · {worker}/{job}"),
                        vec![
                            "logs".into(),
                            worker,
                            job,
                            "--follow".into(),
                            "--lines".into(),
                            "1000".into(),
                        ],
                    );
                } else {
                    self.open_text(
                        format!("logs · {worker}"),
                        vec!["logs".into(), worker, "--follow".into()],
                    );
                }
            }
            KeyCode::Char('p') if worker.is_some() => {
                let worker = worker.unwrap_or_default();
                self.open_text(
                    format!("ps · {worker}"),
                    vec!["ps".into(), worker, "--long".into()],
                );
            }
            KeyCode::Char('c') if worker.is_some() => self.ask(
                PromptKind::Start(worker.unwrap_or_default()),
                "start job: command",
                "",
                "runs from ~ on the worker; output goes to its log",
            ),
            KeyCode::Char('s') if worker.is_some() => {
                self.suspend = Some(vec!["ssh".into(), worker.unwrap_or_default()])
            }
            _ => {}
        }
    }

    fn create_from_selected_profile(&mut self) {
        let Some(Sub::Profiles(profiles)) = self.stack.last() else {
            return;
        };
        let selected = profiles.state.selected().unwrap_or(0);
        if selected < profiles.rows.len() {
            self.create_worker = Some(CreateWorker::from_profile_rows(
                profiles.rows.clone(),
                selected,
            ));
        }
    }

    fn profiles_key(&mut self, k: KeyEvent) {
        let Some(Sub::Profiles(profiles)) = self.stack.last_mut() else {
            return;
        };
        let selected = profiles.state.selected().unwrap_or(0);
        let count = profiles.rows.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q' | 'h') | KeyCode::Backspace => {
                self.stack.pop();
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                profiles.state.select(Some((selected + 1).min(count - 1)));
            }
            KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                profiles.state.select(Some(selected.saturating_sub(1)));
            }
            KeyCode::Home | KeyCode::Char('g') if count > 0 => profiles.state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') if count > 0 => {
                profiles.state.select(Some(count - 1))
            }
            KeyCode::Char('c') => self.create_from_selected_profile(),
            KeyCode::Enter | KeyCode::Char('i') => {
                if let Some(name) = profiles
                    .rows
                    .get(selected)
                    .and_then(|row| row["name"].as_str())
                {
                    let title = format!("profile · {name}");
                    let args = s(&["profiles", name]);
                    let mut details = self.text_stream(title, args);
                    details.wrap = true;
                    details.follow = false;
                    self.stack.push(Sub::Text(details));
                }
            }
            _ => {}
        }
    }

    fn text_key(&mut self, k: KeyEvent) {
        let Some(Sub::Text(v)) = self.stack.last_mut() else {
            return;
        };
        let page = v.height.max(2) - 1;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let down = |v: &mut TextView, by: usize| {
            v.top += by;
            // The draw clamps and turns following back on at the bottom.
        };
        let up = |v: &mut TextView, by: usize| {
            v.follow = false;
            v.top = v.top.saturating_sub(by);
        };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Char('h') => {
                if k.code == KeyCode::Esc && !v.filter.is_empty() {
                    v.filter.clear();
                    v.follow = true;
                } else {
                    self.stack.pop();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => down(v, 1),
            KeyCode::Up | KeyCode::Char('k') => up(v, 1),
            KeyCode::PageDown => down(v, page),
            KeyCode::PageUp => up(v, page),
            KeyCode::Char('d') if ctrl => down(v, page / 2),
            KeyCode::Char('u') if ctrl => up(v, page / 2),
            KeyCode::Home | KeyCode::Char('g') => up(v, usize::MAX),
            KeyCode::End | KeyCode::Char('G') => v.follow = true,
            KeyCode::Char('f' | ' ') => v.follow = !v.follow,
            KeyCode::Char('J') => {
                if let Some(worker) = v.worker.clone() {
                    self.open_jobs(Some(worker));
                }
            }
            KeyCode::Char('w') => v.wrap = !v.wrap,
            KeyCode::Char('c') => {
                v.lines.clear();
                v.top = 0;
            }
            KeyCode::Char('/') => {
                let f = v.filter.clone();
                self.ask(
                    PromptKind::TextFilter,
                    "filter lines",
                    &f,
                    "case-insensitive text",
                );
            }
            _ => {}
        }
    }

    fn prompt_key(&mut self, k: KeyEvent) {
        let Some(p) = self.prompt.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => {
                let kind = p.kind.clone();
                self.prompt = None;
                match kind {
                    PromptKind::Filter => {
                        self.filter.clear();
                        let keep = self.selected().map(|w| w.name.clone());
                        self.reselect(keep);
                    }
                    PromptKind::TextFilter => {
                        if let Some(Sub::Text(v)) = self.stack.last_mut() {
                            v.filter.clear();
                        }
                    }
                    _ => {}
                }
                return;
            }
            KeyCode::Enter => {
                let p = self.prompt.take().expect("prompt");
                self.submit(p);
                return;
            }
            _ => create::edit(&mut p.input, &mut p.cursor, k),
        }
        // Filters apply while typing.
        let (kind, input) = (p.kind.clone(), p.input.clone());
        match kind {
            PromptKind::Filter => {
                let keep = self.selected().map(|w| w.name.clone());
                self.filter = input;
                self.reselect(keep);
            }
            PromptKind::TextFilter => {
                if let Some(Sub::Text(v)) = self.stack.last_mut() {
                    v.filter = input;
                    v.follow = true;
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self, p: Prompt) {
        let input = p.input.trim().to_string();
        match p.kind {
            PromptKind::Filter | PromptKind::TextFilter => {}
            PromptKind::Extend(w) if !input.is_empty() => {
                self.act(
                    vec!["extend".into(), w.clone(), "--ttl".into(), input.clone()],
                    format!("extend {w} by {input}"),
                );
            }
            PromptKind::Topic(w) => {
                let mut args = vec!["topic".to_string(), w.clone()];
                if !self.workers.iter().any(|x| x.name == w && x.mine) {
                    args.push("--force".into());
                }
                args.push(input);
                self.act(args, format!("topic of {w}"));
            }
            PromptKind::Claim(w) => {
                let mut args = vec!["claim".to_string(), w.clone()];
                if !self.workers.iter().any(|x| x.name == w && x.mine) {
                    args.push("--force".into());
                }
                if !input.is_empty() {
                    args.extend(["--topic".into(), input]);
                }
                self.act(args, format!("claim {w}"));
            }
            PromptKind::Start(w) if !input.is_empty() => {
                let label = format!("start on {w}");
                self.act(vec!["start".into(), w, "--".into(), input], label);
                self.reload_jobs();
            }
            PromptKind::Command if !input.is_empty() => match split_words(&input) {
                Ok(mut words) => {
                    if words.first().map(String::as_str) == Some("rworker") {
                        words.remove(0);
                    }
                    let interactive =
                        matches!(words.first().map(String::as_str), Some("ssh" | "tui"))
                            || words.iter().any(|w| w == "--ssh")
                            || (words.first().map(String::as_str) == Some("prune")
                                && !words.iter().any(|w| {
                                    matches!(
                                        w.as_str(),
                                        "-y" | "--yes" | "-n" | "--dry-run" | "--json" | "-j"
                                    )
                                }));
                    if interactive {
                        self.suspend = Some(words);
                    } else {
                        self.open_text(input, words);
                    }
                }
                Err(e) => self.say(e, true),
            },
            _ => {}
        }
    }
}

/// The prefix trim for `logs WORKER [JOB] ...`.
fn log_prefix(args: &[String]) -> Option<(regex::Regex, &'static str)> {
    let positional: Vec<&String> = args
        .iter()
        .skip(1)
        .take_while(|a| !a.starts_with('-'))
        .collect();
    if args.first().map(String::as_str) != Some("logs") {
        return None;
    }
    let (pattern, rep) = match positional.as_slice() {
        [w] => (format!(r"^(\x1b\[[0-9;]*m){}/", regex::escape(w)), "$1"),
        [w, j] => (
            format!(
                r"^\x1b\[[0-9;]*m{}/{}\x1b\[0m ",
                regex::escape(w),
                regex::escape(j)
            ),
            "",
        ),
        _ => return None,
    };
    regex::Regex::new(&pattern).ok().map(|re| (re, rep))
}

fn s(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

/// Shell-like words: whitespace splits, quotes and backslashes group.
pub fn split_words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut any = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                any = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                any = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x) => cur.push(x),
                            None => return Err("unterminated \"".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            '\\' => {
                any = true;
                if let Some(x) = chars.next() {
                    cur.push(x);
                }
            }
            c if c.is_whitespace() => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                any = true;
                cur.push(c);
            }
        }
    }
    if any {
        words.push(cur);
    }
    Ok(words)
}

/// Put TEXT on the clipboard of the terminal (OSC 52).
fn copy(text: &str) {
    use std::io::Write;
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut enc = String::new();
    for chunk in text.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                enc.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                enc.push('=');
            }
        }
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{enc}\x07");
    let _ = out.flush();
}

/// The cached worker list of the completions, for the first frame.
fn cached(ctx: &Ctx) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(ctx.ls_cache()).ok()?;
    match serde_json::from_str(&text).ok()? {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

pub(super) fn run(ctx: &Ctx, interval: Duration, view: View) -> std::io::Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel();
    let paused = Arc::new(AtomicBool::new(false));
    let parked = Arc::new(AtomicBool::new(false));
    let mut app = App {
        control_host: ctx.control_host.clone(),
        owner: ctx.owner(),
        icons: ctx.icons.get(),
        workers: Vec::new(),
        show_history: false,
        history: Vec::new(),
        history_loading: false,
        table: TableState::default(),
        mouse_table: None,
        mouse_profile_create: None,
        mouse_text: None,
        log_divider: None,
        log_drag: None,
        log_width: None,
        log_height: None,
        preview: None,
        filter: String::new(),
        mine_only: false,
        refreshing: false,
        inventory_live: false,
        refreshed: None,
        stale: false,
        refresh_error: None,
        stack: Vec::new(),
        prompt: None,
        create_worker: None,
        confirm: None,
        help: false,
        help_top: 0,
        plain: false,
        flash: None,
        tick: 0,
        pending: HashMap::new(),
        next_id: 0,
        tx: tx.clone(),
        refresh: refresh_tx,
        suspend: None,
        ready_ssh: None,
        quit: false,
    };
    if let Some(list) = cached(ctx) {
        app.set_workers(&list);
        app.stale = true;
    }
    proc::refresher(tx.clone(), interval, refresh_rx);
    app.open_view(view);
    proc::input(tx, paused.clone(), parked.clone());

    // A closed terminal or a kill ends the TUI cleanly, stopping its
    // streams; SIGINT (from an ssh session sharing the terminal) does not.
    let stop = Arc::new(AtomicBool::new(false));
    let interrupt = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGHUP, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&interrupt));

    let mut term = ratatui::init();
    let mouse = MouseCapture;
    let result = (|| -> std::io::Result<()> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        let mut last_jobs_refresh = Instant::now();
        loop {
            if last_jobs_refresh.elapsed() >= interval {
                if matches!(app.stack.last(), Some(Sub::Jobs(_))) {
                    app.reload_jobs();
                } else if app.stack.is_empty() {
                    app.refresh_preview_jobs();
                }
                last_jobs_refresh = Instant::now();
            }
            term.draw(|f| app.draw(f))?;
            app.sync_preview();
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => {
                    app.handle(msg);
                    // Take everything that is queued before drawing again.
                    let t0 = Instant::now();
                    while t0.elapsed() < Duration::from_millis(30) {
                        match rx.try_recv() {
                            Ok(m) => app.handle(m),
                            Err(_) => break,
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            app.tick = app.tick.wrapping_add(1);
            interrupt.store(false, Ordering::SeqCst);
            if app.quit || stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(args) = app.suspend.take().or_else(|| app.ready_ssh.take()) {
                paused.store(true, Ordering::SeqCst);
                let t0 = Instant::now();
                while !parked.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(1) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                execute!(std::io::stdout(), DisableMouseCapture)?;
                ratatui::restore();
                execute!(std::io::stdout(), Show)?;
                let status = proc::interactive(&args);
                if !matches!(status, Ok(Some(0))) {
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let _ = write!(
                        out,
                        "\n[rworker {} ended: {}] press Enter to return ",
                        if args.first().is_some_and(|arg| arg == "__connect-created") {
                            format!("ssh {}", args[1])
                        } else {
                            args.join(" ")
                        },
                        match &status {
                            Ok(Some(c)) => format!("exit {c}"),
                            Ok(None) => "killed".into(),
                            Err(e) => e.to_string(),
                        }
                    );
                    let _ = out.flush();
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                }
                term = ratatui::init();
                execute!(std::io::stdout(), EnableMouseCapture)?;
                term.clear()?;
                paused.store(false, Ordering::SeqCst);
                let _ = app.refresh.send(());
            }
        }
    })();
    drop(mouse);
    ratatui::restore();
    let _ = execute!(std::io::stdout(), Show);
    app.preview = None;
    // Stop the streams before the process ends.
    app.stack.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_prefixes() {
        let args = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let line = "\x1b[1;31mw1/job-a\x1b[0m hello";
        let (re, rep) = log_prefix(&args(&["logs", "w1", "--follow"])).unwrap();
        assert_eq!(re.replace(line, rep), "\x1b[1;31mjob-a\x1b[0m hello");
        let (re, rep) = log_prefix(&args(&["logs", "w1", "job-a", "--follow"])).unwrap();
        assert_eq!(re.replace(line, rep), "hello");
        assert!(log_prefix(&args(&["logs", "--follow"])).is_none());
        assert!(log_prefix(&args(&["inspect", "w1"])).is_none());
    }

    #[test]
    fn words() {
        assert_eq!(
            split_words(r#"sync w 'a b' "c \" d" e\ f"#).unwrap(),
            ["sync", "w", "a b", "c \" d", "e f"]
        );
        assert_eq!(split_words("--topic ''").unwrap(), ["--topic", ""]);
        assert!(split_words("'x").is_err());
    }
}
