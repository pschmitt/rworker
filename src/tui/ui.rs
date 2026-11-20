//! Drawing: a header line, the current view, a one-line key hint footer,
//! and overlays (prompt, confirmation, help).

use super::ansi;
use super::app::{App, Jobs, Sub, TextView, Urgency, Worker};
use crate::{paint, table::glyph};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn key_hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, what)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", dim()));
        }
        spans.push(Span::styled(
            (*k).to_string(),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {what}"), dim()));
    }
    Line::from(spans)
}

fn agent_cell(agent: &str, icons: bool) -> Line<'static> {
    if agent.is_empty() {
        return Line::styled("-", dim());
    }
    let icon = glyph(agent);
    let mut label = if icons && !icon.is_empty() {
        let mut icon = ansi::line(&paint::agents(icon));
        icon.push_span(" ");
        icon
    } else {
        Line::default()
    };
    label.push_span(agent.to_owned());
    label
}

enum WorkerDetails {
    Sidebar(Rect),
    Below(Rect),
    Hidden,
}

fn workers_layout(area: Rect, has_selection: bool) -> (Rect, WorkerDetails) {
    if !has_selection {
        return (area, WorkerDetails::Hidden);
    }
    if area.width >= 116 && area.height >= 10 {
        let sidebar_width = (area.width / 3).clamp(36, 56);
        let [table, details] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(sidebar_width)])
                .areas(area);
        (table, WorkerDetails::Sidebar(details))
    } else if area.height >= 14 {
        let height = if area.width < 80 { 7 } else { 5 };
        let [table, details] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
        (table, WorkerDetails::Below(details))
    } else {
        (area, WorkerDetails::Hidden)
    }
}

fn draw_worker_details(f: &mut Frame, details: WorkerDetails, w: &Worker) {
    let (area, sidebar) = match details {
        WorkerDetails::Sidebar(area) => (area, true),
        WorkerDetails::Below(area) => (area, false),
        WorkerDetails::Hidden => return,
    };
    let field = |label: &str, value: &str| {
        [
            Span::styled(format!("{label} "), dim()),
            Span::raw(if value.is_empty() { "-" } else { value }.to_owned()),
        ]
    };
    let fields = [
        field("owner", &w.owner),
        field("profile", &w.profile),
        field("flavor", &w.flavor),
        field("ip", &w.ips),
        field("agent id", &w.agent_id),
        field("cwd", &w.cwd),
    ];
    let lines: Vec<Line> = if sidebar {
        fields
            .into_iter()
            .map(|field| Line::from(Vec::from(field)))
            .collect()
    } else {
        fields
            .chunks(2)
            .map(|pair| {
                let mut spans = Vec::from(pair[0].clone());
                spans.push(Span::raw("   "));
                spans.extend(pair[1].clone());
                Line::from(spans)
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(dim())
                .title(Span::styled(
                    format!(" {} ", w.name),
                    Style::new().fg(ACCENT),
                )),
        ),
        area,
    );
}

fn status_style(status: &str) -> Style {
    match status {
        "ACTIVE" | "READY" => Style::new().fg(Color::Green),
        "BUILD" | "BUILDING" | "BOOTING" | "REBOOT" => Style::new().fg(Color::Yellow),
        "ERROR" | "DELETED" | "SHUTOFF" => Style::new().fg(Color::Red).bold(),
        _ => Style::new(),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4)).max(10);
    let h = height.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 3,
        width: w,
        height: h,
    }
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s ago")
    } else {
        format!("{}m ago", s / 60)
    }
}

impl App {
    pub fn draw(&mut self, f: &mut Frame) {
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, head);
        let mut stack = std::mem::take(&mut self.stack);
        match stack.last_mut() {
            None => self.draw_workers(f, body),
            Some(Sub::Jobs(j)) => draw_jobs(f, body, j, self.tick),
            Some(Sub::Text(v)) => draw_text(f, body, v, self.tick),
        }
        self.stack = stack;
        self.draw_footer(f, foot);
        if self.help {
            self.draw_help(f);
        }
        if let Some(c) = &self.confirm {
            let lines: Vec<Line> = c
                .text
                .lines()
                .map(|l| Line::from(l.to_string()))
                .chain([
                    Line::default(),
                    key_hints(&[("y", "yes"), ("any other key", "no")]),
                ])
                .collect();
            let width = c
                .text
                .lines()
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(20) as u16
                + 4;
            let area = centered(f.area(), width.max(36), lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(Color::Red))
                        .title(" confirm "),
                ),
                area,
            );
        }
        if let Some(p) = &self.prompt {
            let area = centered(f.area(), 72, 3);
            f.render_widget(Clear, area);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(format!(" {} ", p.title))
                .title_bottom(Line::styled(format!(" {} ", p.hint), dim()));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let before: String = p.input.chars().take(p.cursor).collect();
            let shift = (before.width() as u16).saturating_sub(inner.width.saturating_sub(3));
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("› ", Style::new().fg(ACCENT)),
                    Span::raw(p.input.clone()),
                ]))
                .scroll((0, shift)),
                inner,
            );
            f.set_cursor_position((inner.x + 2 + before.width() as u16 - shift, inner.y));
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mine = self.workers.iter().filter(|w| w.mine).count();
        let mut left = vec![
            Span::styled(" rworker ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled(self.control_host.clone(), Style::new().bold()),
            Span::styled("  ·  ", dim()),
            Span::raw(format!("{} workers", self.workers.len())),
            Span::styled(format!(" ({mine} yours)"), dim()),
        ];
        let tasks = self.pending_actions();
        if tasks > 0 {
            left.push(Span::styled("  ·  ", dim()));
            left.push(Span::styled(
                format!(
                    "{} {tasks} running",
                    SPINNER[self.tick as usize % SPINNER.len()]
                ),
                Style::new().fg(Color::Yellow),
            ));
        }
        let right = if let Some(e) = &self.refresh_error {
            Span::styled(format!("⚠ {e} "), Style::new().fg(Color::Red))
        } else if self.refreshing {
            Span::styled(
                format!(
                    "{} refreshing ",
                    SPINNER[self.tick as usize % SPINNER.len()]
                ),
                dim(),
            )
        } else if let Some((at, took)) = self.refreshed {
            Span::styled(
                format!(
                    "updated {} · {:.1}s ",
                    ago(at.elapsed()),
                    took.as_secs_f64()
                ),
                dim(),
            )
        } else if self.stale {
            Span::styled("cached list ", dim())
        } else {
            Span::raw("")
        };
        let [l, r] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(right.width() as u16),
        ])
        .areas(area);
        f.render_widget(Line::from(left), l);
        f.render_widget(Line::from(right).alignment(Alignment::Right), r);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some(fl) = self.flash_live() {
            let style = if fl.error {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::Green)
            };
            f.render_widget(Line::styled(format!(" {}", fl.text), style), area);
            return;
        }
        let hints = match self.stack.last() {
            None => key_hints(&[
                ("⏎", "jobs"),
                ("l", "logs"),
                ("L", "all logs"),
                ("s", "ssh"),
                ("i", "inspect"),
                ("e", "extend"),
                ("d", "down"),
                ("n", "new"),
                ("/", "filter"),
                ("?", "help"),
            ]),
            Some(Sub::Jobs(_)) => key_hints(&[
                ("⏎", "job log"),
                ("l", "all logs"),
                ("p", "ps"),
                ("n", "start job"),
                ("s", "ssh"),
                ("r", "reload"),
                ("esc", "back"),
            ]),
            Some(Sub::Text(_)) => key_hints(&[
                ("↑↓", "scroll"),
                ("G", "follow"),
                ("/", "filter"),
                ("w", "wrap"),
                ("c", "clear"),
                ("esc", "back"),
            ]),
        };
        f.render_widget(
            hints,
            Rect {
                x: area.x + 1,
                width: area.width.saturating_sub(1),
                ..area
            },
        );
    }

    fn draw_workers(&mut self, f: &mut Frame, area: Rect) {
        let now = crate::time::now() as f64;
        let selected = self.selected().map(|w| w.name.clone());
        let visible = self.visible();
        let mut title = vec![Span::styled(" workers ", Style::new().bold())];
        if self.mine_only {
            title.push(Span::styled("[yours] ", Style::new().fg(Color::Yellow)));
        }
        if !self.filter.is_empty() {
            title.push(Span::styled(
                format!("[/{}] ", self.filter),
                Style::new().fg(Color::Yellow),
            ));
        }
        if !self.filter.is_empty() || self.mine_only {
            title.push(Span::styled(
                format!("{}/{} ", visible.len(), self.workers.len()),
                dim(),
            ));
        }
        let (table_area, details) = workers_layout(area, selected.is_some() && !visible.is_empty());
        let wide = table_area.width >= 150;
        let mask = [
            table_area.width >= 48, // row marker
            true,                   // name
            table_area.width >= 70, // kind
            true,                   // status
            table_area.width >= 55, // agent
            table_area.width >= 37, // topic
            table_area.width >= 48, // expires
            table_area.width >= 82, // age
            wide,
            wide,
            wide,
        ];
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(dim())
            .title(Line::from(title));

        if visible.is_empty() {
            let msg = if self.workers.is_empty() {
                if self.refreshed.is_none() && !self.stale {
                    "loading…"
                } else {
                    "No workers. Press n to create one."
                }
            } else {
                "Nothing matches the filter (esc clears it)."
            };
            f.render_widget(
                Paragraph::new(Line::styled(msg, dim()))
                    .alignment(Alignment::Center)
                    .block(block),
                table_area,
            );
            return;
        }

        let mut header = vec![
            "", "NAME", "KIND", "STATUS", "AGENT", "TOPIC", "EXPIRES", "AGE",
        ];
        if wide {
            header.extend(["OWNER", "PROFILE", "IP"]);
        }
        let rows: Vec<Row> = visible
            .iter()
            .map(|w| {
                let (exp, urg) = w.expiry(now);
                let exp_style = match urg {
                    Urgency::Now => Style::new().fg(Color::Red),
                    Urgency::Soon => Style::new().fg(Color::Indexed(208)),
                    Urgency::Idle => dim(),
                    Urgency::None => Style::new(),
                };
                let fresh = self.new_at.contains_key(&w.name);
                let mark = if fresh {
                    Span::styled("+", Style::new().fg(Color::Green).bold())
                } else if w.mine {
                    Span::styled("●", Style::new().fg(ACCENT))
                } else {
                    Span::raw(" ")
                };
                let agent = agent_cell(&w.agent, self.icons);
                let kind_style = if w.kind == "pool" {
                    dim()
                } else {
                    Style::new()
                };
                let topic = if w.topic.is_empty() {
                    Span::styled("-", dim())
                } else {
                    Span::raw(w.topic.clone())
                };
                let mut cells = vec![
                    Cell::from(mark),
                    Cell::from(Span::styled(w.name.clone(), Style::new().fg(ACCENT))),
                    Cell::from(Span::styled(w.kind.clone(), kind_style)),
                    Cell::from(Span::styled(w.status.clone(), status_style(&w.status))),
                    Cell::from(agent),
                    Cell::from(topic),
                    Cell::from(Span::styled(exp, exp_style)),
                    Cell::from(Span::styled(w.age(now), dim())),
                ];
                if wide {
                    cells.push(Cell::from(Span::styled(
                        if w.owner.is_empty() {
                            "-".into()
                        } else {
                            w.owner.clone()
                        },
                        dim(),
                    )));
                    cells.push(Cell::from(Span::styled(w.profile.clone(), dim())));
                    cells.push(Cell::from(Span::styled(w.ips.clone(), dim())));
                }
                Row::new(
                    cells
                        .into_iter()
                        .zip(mask)
                        .filter_map(|(cell, show)| show.then_some(cell)),
                )
            })
            .collect();
        let name_w = visible
            .iter()
            .map(|w| w.name.width())
            .max()
            .unwrap_or(4)
            .clamp(4, usize::from((table_area.width / 3).max(4))) as u16;
        let agent_w = visible
            .iter()
            .map(|w| {
                w.agent.width()
                    + if self.icons && !glyph(&w.agent).is_empty() {
                        2
                    } else {
                        0
                    }
            })
            .max()
            .unwrap_or(5)
            .clamp(5, 16) as u16;
        let mut widths = vec![
            Constraint::Length(1),
            Constraint::Length(name_w),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(agent_w),
            Constraint::Fill(1),
            Constraint::Length(11),
            Constraint::Length(7),
        ];
        if wide {
            widths.extend([
                Constraint::Length(16),
                Constraint::Length(14),
                Constraint::Length(31),
            ]);
        }
        let table = Table::new(
            rows,
            widths
                .into_iter()
                .zip(mask)
                .filter_map(|(width, show)| show.then_some(width)),
        )
        .header(
            Row::new(
                header
                    .into_iter()
                    .zip(mask)
                    .filter_map(|(label, show)| show.then_some(label)),
            )
            .style(Style::new().add_modifier(Modifier::BOLD | Modifier::DIM)),
        )
        .block(block)
        .column_spacing(if table_area.width < 82 { 1 } else { 2 })
        .row_highlight_style(
            Style::new()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        );
        f.render_stateful_widget(table, table_area, &mut self.table);

        if let Some(worker) = selected.and_then(|name| self.workers.iter().find(|w| w.name == name))
        {
            draw_worker_details(f, details, worker);
        }
    }

    fn draw_help(&self, f: &mut Frame) {
        let section = |t: &str| Line::styled(t.to_string(), Style::new().fg(ACCENT).bold());
        let row = |k: &str, what: &str| {
            Line::from(vec![
                Span::styled(format!(" {k:<13}"), Style::new().bold()),
                Span::raw(what.to_string()),
            ])
        };
        let left = vec![
            section("Workers"),
            row("↑↓ j k g G", "move"),
            row("⏎ / o", "jobs of the worker"),
            row("l", "follow its logs"),
            row("L", "follow all workers' logs"),
            row("E", "lifecycle events"),
            row("i / I", "inspect (I: boot metadata)"),
            row("p", "processes of its jobs"),
            row("s", "ssh (exit comes back)"),
            row("e  t  c", "extend · topic · claim"),
            row("d", "down (asks first)"),
            row("n", "new worker (up)"),
            row("X", "prune yours (asks first)"),
            row("K  N", "niks3 token · nix-cache"),
            row("b  y", "builder line · copy name"),
            row("P", "profiles"),
            row("/  m", "filter · only yours"),
            row("r", "refresh now"),
        ];
        let right = vec![
            section("Jobs"),
            row("⏎", "follow the job's log"),
            row("n", "start a job"),
            row("l  p  s", "all logs · ps · ssh"),
            row("r", "reload"),
            Line::default(),
            section("Logs and output"),
            row("↑↓ PgUp PgDn", "scroll (pauses)"),
            row("G / f", "follow / toggle"),
            row("/", "filter lines"),
            row("w  c", "wrap · clear"),
            Line::default(),
            section("Anywhere"),
            row(":", "any rworker action"),
            row("", "(sync, cp, exec, …)"),
            row("esc / q", "back (q on the list quits)"),
            row("ctrl-c", "quit"),
        ];
        let rows = left.len().max(right.len()) as u16;
        let area = centered(f.area(), 92, rows + 2);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(" keys ")
            .title_bottom(Line::styled(" any key closes ", dim()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(inner);
        f.render_widget(Paragraph::new(left), l);
        f.render_widget(Paragraph::new(right), r);
    }
}

fn draw_jobs(f: &mut Frame, area: Rect, j: &mut Jobs, tick: u64) {
    let mut title = vec![
        Span::styled(" jobs · ", Style::new().bold()),
        Span::styled(format!("{} ", j.worker), Style::new().fg(ACCENT).bold()),
    ];
    if j.loading.is_some() {
        title.push(Span::styled(
            format!("{} ", SPINNER[tick as usize % SPINNER.len()]),
            dim(),
        ));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(title));
    if let Some(e) = &j.error {
        f.render_widget(
            Paragraph::new(Line::styled(e.clone(), Style::new().fg(Color::Red)))
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    if j.rows.is_empty() {
        let msg = if j.loading.is_some() {
            "loading…"
        } else {
            "No jobs. Press n to start one, l to follow the worker's logs."
        };
        f.render_widget(
            Paragraph::new(Line::styled(msg, dim()))
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return;
    }
    let rows: Vec<Row> = j
        .rows
        .iter()
        .map(|[name, state, started]| {
            let st = if state == "running" {
                Style::new().fg(Color::Green)
            } else if state == "exit 0" {
                dim()
            } else if state.starts_with("exit") {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::Yellow)
            };
            Row::new(vec![
                Cell::from(Span::styled(name.clone(), Style::new().fg(ACCENT))),
                Cell::from(Span::styled(state.clone(), st)),
                Cell::from(Span::styled(started.clone(), dim())),
            ])
        })
        .collect();
    let name_w = j
        .rows
        .iter()
        .map(|r| r[0].width())
        .max()
        .unwrap_or(3)
        .max(3) as u16;
    let table = Table::new(
        rows,
        [
            Constraint::Length(name_w),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new(["JOB", "STATE", "STARTED"])
            .style(Style::new().add_modifier(Modifier::BOLD | Modifier::DIM)),
    )
    .block(block)
    .column_spacing(2)
    .row_highlight_style(
        Style::new()
            .bg(Color::Indexed(236))
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(table, area, &mut j.state);
}

fn draw_text(f: &mut Frame, area: Rect, v: &mut TextView, tick: u64) {
    let mut title = vec![Span::styled(format!(" {} ", v.title), Style::new().bold())];
    match &v.exit {
        None => title.push(Span::styled(
            format!("{} ", SPINNER[tick as usize % SPINNER.len()]),
            Style::new().fg(Color::Green),
        )),
        Some(Some(0)) => {}
        Some(Some(c)) => title.push(Span::styled(
            format!("[exit {c}] "),
            Style::new().fg(Color::Red),
        )),
        Some(None) => title.push(Span::styled("[ended] ", dim())),
    }
    if !v.filter.is_empty() {
        title.push(Span::styled(
            format!("[/{}] ", v.filter),
            Style::new().fg(Color::Yellow),
        ));
    }
    let mut flags = Vec::new();
    if !v.follow {
        flags.push("paused");
    }
    if v.wrap {
        flags.push("wrap");
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(title))
        .title_bottom(Line::styled(
            if flags.is_empty() {
                String::new()
            } else {
                format!(" {} ", flags.join(" · "))
            },
            Style::new().fg(Color::Yellow),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let height = inner.height as usize;
    v.height = height;
    let needle = v.filter.to_lowercase();
    let shown: Vec<&(String, bool)> = v
        .lines
        .iter()
        .filter(|(l, _)| needle.is_empty() || ansi::strip(l).to_lowercase().contains(&needle))
        .collect();
    if shown.is_empty() {
        let msg = if v.exit.is_none() {
            "waiting for output…"
        } else {
            "no output"
        };
        f.render_widget(Paragraph::new(Line::styled(msg, dim())), inner);
        return;
    }
    let width = inner.width.max(1) as usize;
    let rows_of = |l: &str| -> usize {
        if v.wrap {
            ansi::strip(l).width().div_ceil(width).max(1)
        } else {
            1
        }
    };
    // The first line to show: following keeps the last lines in view.
    let max_top = {
        let mut used = 0;
        let mut i = shown.len();
        while i > 0 && used + rows_of(&shown[i - 1].0) <= height {
            used += rows_of(&shown[i - 1].0);
            i -= 1;
        }
        i
    };
    if v.follow || v.top >= max_top {
        v.top = max_top;
        v.follow = true;
    }
    let lines: Vec<Line> = shown[v.top..]
        .iter()
        .take(height)
        .map(|(l, err)| {
            let line = ansi::line(l);
            if *err && !l.contains('\x1b') {
                line.patch_style(Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC))
            } else {
                line
            }
        })
        .collect();
    let mut p = Paragraph::new(Text::from(lines));
    if v.wrap {
        p = p.wrap(Wrap { trim: false });
    }
    f.render_widget(p, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_follow_terminal_geometry() {
        let area = Rect::new(0, 1, 120, 28);
        let (table, WorkerDetails::Sidebar(sidebar)) = workers_layout(area, true) else {
            panic!("expected sidebar on wide terminal");
        };
        assert_eq!(sidebar.x, table.right());
        assert_eq!(sidebar.height, table.height);

        let area = Rect::new(0, 1, 72, 24);
        let (table, WorkerDetails::Below(details)) = workers_layout(area, true) else {
            panic!("expected details under the table");
        };
        assert_eq!(details.y, table.bottom());
        assert_eq!(details.width, table.width);

        let area = Rect::new(0, 1, 72, 10);
        assert!(
            matches!(workers_layout(area, true), (table, WorkerDetails::Hidden) if table == area)
        );
        let area = Rect::new(0, 1, 160, 28);
        assert!(
            matches!(workers_layout(area, false), (table, WorkerDetails::Hidden) if table == area)
        );
    }

    #[test]
    fn agent_cell_uses_cli_icon_palette() {
        let label = agent_cell("claude", true);
        assert_eq!(label.spans[0].content, glyph("claude"));
        assert_eq!(label.spans[0].style.fg, Some(Color::Rgb(217, 119, 87)));
        assert_eq!(label.spans.last().unwrap().content, "claude");
        assert_eq!(agent_cell("claude", false).spans[0].content, "claude");
    }
}
