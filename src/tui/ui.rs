//! Drawing: a header line, the current view, a one-line key hint footer,
//! and overlays (prompt, confirmation, help).

use super::ansi;
use super::app::{App, Jobs, LogDivider, Sub, TextView, Urgency};
use crate::{
    paint,
    table::{COLUMNS, WIDE_COLUMNS, glyph},
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
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

enum WorkerPane {
    Sidebar(Rect),
    Below(Rect),
    Hidden,
}

pub(super) fn log_size(total: u16, requested: u16, direction: Direction) -> u16 {
    match direction {
        Direction::Horizontal => requested.clamp(24, total - 36),
        Direction::Vertical => requested.clamp(4, total - 5),
    }
}

fn workers_layout(
    area: Rect,
    has_selection: bool,
    log_width: Option<u16>,
    log_height: Option<u16>,
) -> (Rect, WorkerPane) {
    if !has_selection {
        return (area, WorkerPane::Hidden);
    }
    if area.width >= 116 && area.height >= 10 {
        let sidebar_width = log_size(
            area.width,
            log_width.unwrap_or_else(|| (area.width / 3).clamp(36, 56)),
            Direction::Horizontal,
        );
        let [table, details] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(sidebar_width)])
                .areas(area);
        (table, WorkerPane::Sidebar(details))
    } else if area.height >= 14 {
        let height = log_size(
            area.height,
            log_height.unwrap_or(if area.width < 80 { 7 } else { 5 }),
            Direction::Vertical,
        );
        let [table, details] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
        (table, WorkerPane::Below(details))
    } else {
        (area, WorkerPane::Hidden)
    }
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
        self.mouse_table = None;
        self.mouse_text = None;
        self.log_divider = None;
        let mut stack = std::mem::take(&mut self.stack);
        match stack.last_mut() {
            None => self.draw_workers(f, body),
            Some(Sub::Jobs(j)) => {
                self.mouse_table = Some(borderless_rows(body));
                draw_jobs(f, body, j);
            }
            Some(Sub::Text(v)) => {
                self.mouse_text = Some(body);
                draw_text(f, body, v, self.tick);
            }
        }
        self.stack = stack;
        if self.log_drag != self.log_divider.as_ref().map(|divider| divider.direction)
            || self.help
            || self.prompt.is_some()
            || self.confirm.is_some()
        {
            self.log_drag = None;
        }
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
        if self.mine_only {
            left.push(Span::styled(
                "  ·  yours only",
                Style::new().fg(Color::Yellow),
            ));
        }
        if !self.filter.is_empty() {
            left.push(Span::styled(
                format!("  ·  /{}", self.filter),
                Style::new().fg(Color::Yellow),
            ));
        }
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
        } else if self.inventory_live {
            Span::styled("live ", Style::new().fg(Color::Green))
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
                ("J", "jobs"),
                (
                    "h",
                    if self.show_history {
                        "hide retired"
                    } else {
                        "show retired"
                    },
                ),
                (
                    "⏎",
                    if self.selected().is_some_and(|worker| worker.retired) {
                        "cached logs"
                    } else {
                        "worker jobs"
                    },
                ),
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
            Some(Sub::Jobs(jobs)) => key_hints(&[
                ("J", "workers"),
                (
                    "d",
                    if jobs.show_finished {
                        "active jobs"
                    } else {
                        "include finished"
                    },
                ),
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
        let (table_area, details) = workers_layout(
            area,
            selected.is_some() && !visible.is_empty(),
            self.log_width,
            self.log_height,
        );
        let wide = table_area.width >= 150;
        let mask = [
            true,                   // name
            true,                   // status
            table_area.width >= 55, // agent
            table_area.width >= 37, // topic
            table_area.width >= 48, // expires
            table_area.width >= 82, // age
            wide,
            wide,
            wide,
        ];
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
                Paragraph::new(Line::styled(msg, dim())).alignment(Alignment::Center),
                table_area,
            );
            return;
        }

        let mut header = COLUMNS.to_vec();
        if wide {
            header.extend(WIDE_COLUMNS);
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
                let agent = agent_cell(&w.agent, self.icons);
                let topic = if w.topic.is_empty() {
                    Span::styled("-", dim())
                } else {
                    Span::raw(w.topic.clone())
                };
                let mut cells = vec![
                    Cell::from(Span::styled(w.name.clone(), Style::new().fg(ACCENT))),
                    Cell::from(Span::styled(
                        w.status.clone(),
                        if w.retired {
                            dim()
                        } else {
                            status_style(&w.status)
                        },
                    )),
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
        let name_cap = (table_area.width / 3)
            .clamp(16, 28)
            .min(table_area.width.saturating_sub(11).max(4));
        let name_w = visible
            .iter()
            .map(|w| w.name.width())
            .max()
            .unwrap_or(4)
            .clamp(4, usize::from(name_cap)) as u16;
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
            Constraint::Length(name_w),
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
        .column_spacing(if table_area.width < 82 { 1 } else { 2 })
        .row_highlight_style(
            Style::new()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        );
        f.render_stateful_widget(table, table_area, &mut self.table);

        self.mouse_table = Some(Rect {
            y: table_area.y.saturating_add(1),
            height: table_area.height.saturating_sub(1),
            ..table_area
        });
        let (pane, border, direction) = match details {
            WorkerPane::Sidebar(pane) => (pane, Rect { width: 1, ..pane }, Direction::Horizontal),
            WorkerPane::Below(pane) => (pane, Rect { height: 1, ..pane }, Direction::Vertical),
            WorkerPane::Hidden => return,
        };
        self.log_divider = Some(LogDivider {
            area,
            border,
            direction,
        });
        let area = pane;
        self.mouse_text = Some(area);
        if let Some((worker, preview)) = &mut self.preview
            && Some(worker.as_str()) == selected.as_deref()
        {
            draw_text(f, area, preview, self.tick);
        } else {
            f.render_widget(
                Paragraph::new("Connecting to logs…").block(Block::bordered().title(" logs ")),
                area,
            );
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
            row("↑↓ k g G", "move"),
            row("J", "toggle Jobs / Workers"),
            row("h", "show or hide retired workers"),
            row("d (Jobs)", "toggle finished jobs"),
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
            row("click / wheel", "select rows or scroll logs"),
            row("drag border", "resize the logs pane"),
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
            row("ctrl-c / alt-shift-q", "quit"),
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

fn borderless_rows(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}

fn draw_jobs(f: &mut Frame, area: Rect, j: &mut Jobs) {
    if j.rows.is_empty() {
        let message = if j.loading.is_some() {
            "Loading jobs…"
        } else if let Some(error) = &j.error {
            error
        } else if j.show_finished {
            "No jobs"
        } else {
            "No active jobs. Press d to include finished jobs."
        };
        f.render_widget(
            Paragraph::new(Line::styled(message, dim())).wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let all_workers = j.worker.is_none();
    let show_start = area.width >= 54;
    let rows: Vec<Row> = j
        .rows
        .iter()
        .map(|[worker, name, state, started]| {
            let st = if state == "running" {
                Style::new().fg(Color::Green)
            } else if state == "exit 0" {
                dim()
            } else if state.starts_with("exit") {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::Yellow)
            };
            let mut cells = Vec::new();
            if all_workers {
                cells.push(Cell::from(Span::styled(worker.clone(), dim())));
            }
            cells.extend([
                Cell::from(Span::styled(name.clone(), Style::new().fg(ACCENT))),
                Cell::from(Span::styled(state.clone(), st)),
            ]);
            if show_start {
                cells.push(Cell::from(Span::styled(started.clone(), dim())));
            }
            Row::new(cells)
        })
        .collect();
    let mut widths = Vec::new();
    let mut header = Vec::new();
    if all_workers {
        widths.push(Constraint::Length((area.width / 3).clamp(12, 22)));
        header.push("WORKER");
    }
    widths.extend([Constraint::Fill(1), Constraint::Length(10)]);
    header.extend(["JOB", "STATE"]);
    if show_start {
        widths.push(Constraint::Length(22));
        header.push("STARTED");
    }
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().add_modifier(Modifier::BOLD | Modifier::DIM)))
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
        i.min(shown.len() - 1)
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
        let (table, WorkerPane::Sidebar(sidebar)) = workers_layout(area, true, None, None) else {
            panic!("expected sidebar on wide terminal");
        };
        assert_eq!(sidebar.x, table.right());
        assert_eq!(sidebar.height, table.height);

        let area = Rect::new(0, 1, 72, 24);
        let (table, WorkerPane::Below(details)) = workers_layout(area, true, None, None) else {
            panic!("expected details under the table");
        };
        assert_eq!(details.y, table.bottom());
        assert_eq!(details.width, table.width);

        let area = Rect::new(0, 1, 72, 10);
        assert!(
            matches!(workers_layout(area, true, None, None), (table, WorkerPane::Hidden) if table == area)
        );
        let area = Rect::new(0, 1, 160, 28);
        assert!(
            matches!(workers_layout(area, false, None, None), (table, WorkerPane::Hidden) if table == area)
        );
    }

    #[test]
    fn log_pane_sizes_clamp_without_crowding_out_workers() {
        let area = Rect::new(0, 1, 120, 28);
        let (table, WorkerPane::Sidebar(logs)) = workers_layout(area, true, Some(100), Some(4))
        else {
            panic!("expected sidebar");
        };
        assert_eq!((table.width, logs.width), (36, 84));

        let area = Rect::new(0, 1, 72, 24);
        let (table, WorkerPane::Below(logs)) = workers_layout(area, true, Some(100), Some(22))
        else {
            panic!("expected stacked logs");
        };
        assert_eq!((table.height, logs.height), (5, 19));
        assert_eq!(log_size(24, 0, Direction::Vertical), 4);
    }

    #[test]
    fn agent_cell_uses_cli_icon_palette() {
        let label = agent_cell("claude", true);
        assert_eq!(label.spans[0].content, glyph("claude"));
        assert_eq!(label.spans[0].style.fg, Some(Color::Rgb(217, 119, 87)));
        assert_eq!(label.spans.last().unwrap().content, "claude");
        assert_eq!(agent_cell("claude", false).spans[0].content, "claude");
        let nix_build = agent_cell("nix build", true);
        assert_eq!(nix_build.spans[0].content, glyph("nixbot"));
        assert_eq!(nix_build.spans[0].style.fg, Some(Color::Rgb(126, 186, 228)));
        assert_eq!(nix_build.spans.last().unwrap().content, "nix build");
    }
}
