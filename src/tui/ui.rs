//! Drawing for the current view, status line, and modal overlays.

use super::ansi;
use super::app::{App, Jobs, LogDivider, Profiles, Sub, TextView, Urgency};
use super::create::{CreateWorker, Field};
use crate::{
    paint,
    table::{COLUMNS, WIDE_COLUMNS, glyph},
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const STATUS_WIDTH: u16 = 8;
const ACCENT: Color = Color::Cyan;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn worker_name(name: &str, icon: Option<&str>) -> Line<'static> {
    let mut label = Line::default();
    if let Some(glyph) = icon {
        label.push_span(Span::styled(glyph.to_owned(), provider_icon_style(glyph)));
        label.push_span(" ");
    }
    label.push_span(Span::styled(name.to_owned(), Style::new().fg(ACCENT)));
    label
}

fn provider_icon_style(glyph: &str) -> Style {
    if glyph == crate::pool_icon::GITLAB {
        Style::new().fg(Color::Rgb(226, 67, 41))
    } else {
        Style::new().fg(ACCENT)
    }
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
    mut status: Rect,
    has_selection: bool,
    log_width: Option<u16>,
    log_height: Option<u16>,
) -> (Rect, WorkerPane, Rect) {
    if !has_selection {
        return (area, WorkerPane::Hidden, status);
    }
    if area.width >= 116 && area.height >= 10 {
        let sidebar_width = log_size(
            area.width,
            log_width.unwrap_or_else(|| (area.width / 3).clamp(36, 56)),
            Direction::Horizontal,
        );
        let [table, mut details] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(sidebar_width)])
                .areas(area);
        status.width = table.width;
        details.height += status.height;
        (table, WorkerPane::Sidebar(details), status)
    } else if area.height >= 14 {
        let height = log_size(
            area.height,
            log_height.unwrap_or(if area.width < 80 { 8 } else { 6 }),
            Direction::Vertical,
        );
        let [table, details] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
        (table, WorkerPane::Below(details), status)
    } else {
        (area, WorkerPane::Hidden, status)
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "ACTIVE" | "READY" => Style::new().fg(Color::Green),
        "BUILD" | "BUILDING" | "BOOTING" | "REBOOT" => Style::new().fg(Color::Yellow),
        "ERROR" | "DOWN…" | "DELETING" | "DELETED" | "SHUTOFF" => {
            Style::new().fg(Color::Red).bold()
        }
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

fn draw_create(f: &mut Frame, form: &CreateWorker, icons: bool) {
    let facts = form.profile_facts();
    let mut rows = Vec::new();
    for field in form.fields() {
        rows.push(Some(field));
        if field == Field::Profile && !facts.is_empty() {
            rows.push(None);
        }
    }
    let area = centered(f.area(), 72, rows.len() as u16 + 3);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" Create worker ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 18 {
        return;
    }

    let error_rows = u16::from(form.error.is_some());
    if let Some(error) = &form.error {
        f.render_widget(
            Paragraph::new(Line::styled(error.as_str(), Style::new().fg(Color::Yellow))),
            Rect::new(inner.x + 1, inner.bottom() - 1, inner.width - 2, 1),
        );
    }
    let visible = usize::from(inner.height - error_rows);
    let focus = rows
        .iter()
        .position(|row| *row == Some(form.focus))
        .unwrap_or(0);
    let start = focus.saturating_sub(visible - 1);
    let label_width = 15;
    let value_width = inner.width - label_width - 2;
    for (position, field) in rows.into_iter().skip(start).take(visible).enumerate() {
        let y = inner.y + position as u16;
        let value_area = Rect::new(inner.x + label_width, y, value_width, 1);
        let Some(field) = field else {
            f.render_widget(
                Paragraph::new(Line::styled(facts.as_str(), dim())),
                value_area,
            );
            continue;
        };
        let active = form.focus == field;
        let style = if active {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::default()
        };
        let marker = if active { "› " } else { "  " };
        let label = Line::styled(format!("{marker}{}", field.label()), style);
        f.render_widget(
            Paragraph::new(label),
            Rect::new(inner.x + 1, y, label_width - 1, 1),
        );
        if let Some((input, placeholder)) = form.input(field) {
            let text = if input.text.is_empty() {
                Line::styled(placeholder, dim())
            } else {
                Line::from(input.text.as_str())
            };
            let before: String = input.text.chars().take(input.cursor).collect();
            let cursor = before.width().min(u16::MAX as usize) as u16;
            let shift = if active {
                cursor.saturating_sub(value_width - 1)
            } else {
                0
            };
            f.render_widget(Paragraph::new(text).scroll((0, shift)), value_area);
            if active {
                f.set_cursor_position((value_area.x + cursor - shift, y));
            }
        } else {
            f.render_widget(
                Paragraph::new(Line::styled(form.choice(field, icons), style)),
                value_area,
            );
        }
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
        let plain = self.plain;
        let [body, mut status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(u16::from(!plain))])
                .areas(f.area());
        self.mouse_table = None;
        self.mouse_profile_create = None;
        self.mouse_text = None;
        self.log_divider = None;
        let mut stack = std::mem::take(&mut self.stack);
        match stack.last_mut() {
            None => status = self.draw_workers(f, body, status),
            Some(Sub::Jobs(j)) => {
                self.mouse_table = Some(borderless_rows(body));
                draw_jobs(f, body, j);
            }
            Some(Sub::Profiles(profiles)) => {
                let table = if !profiles.rows.is_empty() && body.height >= 3 {
                    let [actions, table] =
                        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(body);
                    let width = actions.width.min(16);
                    let button =
                        Rect::new(actions.right().saturating_sub(width), actions.y, width, 1);
                    self.mouse_profile_create = Some(button);
                    f.render_widget(
                        Paragraph::new(" [c] New worker ")
                            .style(Style::new().fg(Color::Black).bg(ACCENT)),
                        button,
                    );
                    table
                } else {
                    body
                };
                self.mouse_table = Some(borderless_rows(table));
                draw_profiles(f, table, profiles, self.icons);
            }
            Some(Sub::Text(v)) => {
                self.mouse_text = Some(body);
                draw_text(f, body, v, plain);
            }
        }
        self.stack = stack;
        if self.log_drag != self.log_divider.as_ref().map(|divider| divider.direction)
            || self.help
            || self.prompt.is_some()
            || self.create_worker.is_some()
            || self.confirm.is_some()
        {
            self.log_drag = None;
        }
        if !plain {
            self.draw_status(f, status);
        }
        if let Some(c) = &self.confirm {
            let lines: Vec<Line> = c.text.lines().map(|l| Line::from(l.to_string())).collect();
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
        if let Some(form) = &self.create_worker {
            draw_create(f, form, self.icons);
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
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
        let spinner = SPINNER[self.tick as usize % SPINNER.len()];
        if tasks > 0 {
            left.push(Span::styled("  ·  ", dim()));
            left.push(Span::styled(
                format!("{spinner} {tasks} running"),
                Style::new().fg(Color::Yellow),
            ));
        }
        let right = if let Some((stage, elapsed)) = self.provisioning_ssh() {
            let style = status_style(stage.split_once(" · ").map_or(stage, |(state, _)| state));
            Span::styled(
                format!("{spinner} {stage} ({}s) ", elapsed.as_secs()),
                style.fg(style.fg.unwrap_or(Color::Yellow)),
            )
        } else if let Some(flash) = self.flash_live() {
            let color = if flash.error {
                Color::Red
            } else {
                Color::Green
            };
            Span::styled(format!("{} ", flash.text), Style::new().fg(color))
        } else if let Some(e) = &self.refresh_error {
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
            Span::raw("")
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
            Constraint::Length(
                right
                    .width()
                    .min(usize::from(area.width.saturating_sub(10))) as u16,
            ),
        ])
        .areas(area);
        f.render_widget(Line::from(left), l);
        let alignment = if right.width() > usize::from(r.width) {
            Alignment::Left
        } else {
            Alignment::Right
        };
        f.render_widget(Line::from(right).alignment(alignment), r);
    }

    fn draw_workers(&mut self, f: &mut Frame, area: Rect, status: Rect) -> Rect {
        let now = crate::time::now() as f64;
        let selected = self.selected().map(|w| w.name.clone());
        let visible = self.visible();
        let (table_area, details, status) = workers_layout(
            area,
            status,
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
                    "No workers."
                }
            } else {
                "Nothing matches the filter (esc clears it)."
            };
            f.render_widget(
                Paragraph::new(Line::styled(msg, dim())).alignment(Alignment::Center),
                table_area,
            );
            return status;
        }

        let mut header = COLUMNS.to_vec();
        if wide {
            header.extend(WIDE_COLUMNS);
        }
        let rows: Vec<Row> = visible
            .iter()
            .map(|w| {
                let status = self.worker_status(w);
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
                    Cell::from(worker_name(&w.name, self.icons.then_some(&w.glyph))),
                    Cell::from(Span::styled(
                        status.to_owned(),
                        if w.retired {
                            dim()
                        } else {
                            status_style(status)
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
            .min(table_area.width.saturating_sub(STATUS_WIDTH + 4).max(4));
        let name_w = visible
            .iter()
            .map(|w| w.name.width() + if self.icons { 2 } else { 0 })
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
            Constraint::Length(STATUS_WIDTH),
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
            WorkerPane::Hidden => return status,
        };
        self.log_divider = Some(LogDivider {
            area,
            border,
            direction,
        });
        let separator = Block::new()
            .borders(match direction {
                Direction::Horizontal => Borders::LEFT,
                Direction::Vertical => Borders::TOP,
            })
            .border_style(dim());
        let area = separator.inner(pane);
        f.render_widget(separator, pane);
        self.mouse_text = Some(area);
        if let Some((worker, preview)) = &mut self.preview
            && Some(worker.as_str()) == selected.as_deref()
        {
            draw_text_content(f, area, preview, self.plain);
        } else if !self.plain {
            f.render_widget(
                Paragraph::new(Line::styled("Connecting to logs…", dim())),
                area,
            );
        }
        status
    }

    fn draw_help(&mut self, f: &mut Frame) {
        let (title, mut keys) = if self.create_worker.is_some() {
            (
                "Create worker",
                vec![
                    ("Tab / ↓", "Next field"),
                    ("Shift-Tab / ↑", "Previous field"),
                    (
                        "← / → / Space",
                        "Change profile or SSH behavior on a selector",
                    ),
                    ("← / →", "Move cursor in a text field"),
                    ("Enter", "Create with the current values"),
                    ("Esc", "Cancel creation"),
                ],
            )
        } else if self.prompt.is_some() {
            (
                "Text input",
                vec![
                    ("← / →", "Move cursor"),
                    ("Enter", "Accept"),
                    ("Esc", "Cancel"),
                ],
            )
        } else if self.confirm.is_some() {
            (
                "Confirm action",
                vec![("y / Enter", "Confirm"), ("Other key", "Cancel")],
            )
        } else {
            match self.stack.last() {
                None => (
                    "Workers",
                    vec![
                        ("↑ / ↓ or k / j", "Select a worker"),
                        ("Home / End or g / G", "First / last worker"),
                        ("PgUp / PgDn", "Move by ten workers"),
                        ("Ctrl-U / Ctrl-D", "Move by ten workers"),
                        (
                            "Enter / o",
                            "Worker jobs, or cached logs for retired workers",
                        ),
                        ("J", "All jobs"),
                        ("l / L", "Worker logs / all logs"),
                        ("E / P", "Lifecycle events / profiles"),
                        ("i / I / p", "Inspect / boot details / job processes"),
                        ("s / b / y", "SSH / builder line / copy name"),
                        ("e / t / C", "Extend / edit topic / claim"),
                        ("K / N", "Renew niks3 token / configure Nix cache"),
                        ("c", "Create a worker"),
                        (
                            "d / Delete / X",
                            "Delete worker / prune yours (confirmation)",
                        ),
                        ("h / m", "Toggle retired workers / only yours"),
                        ("/", "Filter workers"),
                        ("Esc / r / q", "Clear filters / refresh / quit"),
                        ("Click / wheel", "Select workers or scroll log preview"),
                        ("Drag divider", "Resize the log preview"),
                    ],
                ),
                Some(Sub::Jobs(_)) => (
                    "Jobs",
                    vec![
                        ("↑ / ↓ or k / j", "Select a job"),
                        ("g / G", "First / last job"),
                        ("Enter / l", "Selected log / all logs for this worker"),
                        ("c / p / s", "Start job / job processes / SSH"),
                        ("d / r", "Toggle finished jobs / reload"),
                        ("J", "Return to workers"),
                        ("Esc / q / Backspace / h", "Back"),
                        ("Click / wheel", "Select jobs"),
                    ],
                ),
                Some(Sub::Profiles(_)) => (
                    "Profiles",
                    vec![
                        ("↑ / ↓ or k / j", "Select a profile"),
                        ("Home / End or g / G", "First / last profile"),
                        ("Enter / i", "Details for the selected profile"),
                        ("c", "New worker from the selected profile"),
                        ("Esc / q / Backspace / h", "Back"),
                        ("Click / wheel", "Select profiles"),
                    ],
                ),
                Some(Sub::Text(view)) => {
                    let mut keys = vec![
                        ("↑ / ↓ or k / j", "Scroll by a line"),
                        ("PgUp / PgDn", "Scroll by a page"),
                        ("Ctrl-U / Ctrl-D", "Scroll by half a page"),
                        ("Home / g", "Go to top and pause following"),
                        ("End / G", "Resume following at the bottom"),
                        ("Space / f", "Toggle following"),
                        ("/", "Filter lines"),
                        ("w / c", "Toggle wrap / clear output"),
                        ("Mouse wheel", "Scroll"),
                        ("Esc", "Clear the filter or go back"),
                        ("q / Backspace / h", "Back"),
                    ];
                    if view.worker.is_some() {
                        keys.push(("J", "Jobs for this worker"));
                    }
                    ("Logs and output", keys)
                }
            }
        };
        if self.prompt.is_some() || self.create_worker.is_some() {
            keys.extend([
                ("Home / End", "Beginning / end of input"),
                ("Ctrl-A / Ctrl-E", "Beginning / end of input"),
                ("Backspace / Delete", "Delete left / right of cursor"),
                ("Ctrl-W / Ctrl-U", "Delete previous word / to beginning"),
            ]);
        } else if self.confirm.is_none() {
            keys.extend([(":", "Run an rworker command"), ("n", "Toggle plain view")]);
        }
        keys.push(("Ctrl-C / Alt-Shift-Q", "Quit rworker"));
        let compact = f.area().width < 62;
        let row_count = keys.len() as u16 * if compact { 2 } else { 1 };
        let area = centered(f.area(), 78, row_count + 2);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(format!(" {title} · keys "))
            .title_bottom(Line::styled(" ↑ ↓ Scroll · Esc Close ", dim()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines = Vec::new();
        let key_style = Style::new().fg(ACCENT).bold();
        for (key, description) in keys {
            if compact {
                lines.push(Line::styled(format!(" {key}"), key_style));
                lines.push(Line::from(format!("   {description}")));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {key:<24}"), key_style),
                    Span::raw(description),
                ]));
            }
        }
        self.help_top = self
            .help_top
            .min((lines.len() as u16).saturating_sub(inner.height));
        f.render_widget(Paragraph::new(lines).scroll((self.help_top, 0)), inner);
    }
}

fn borderless_rows(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}

fn draw_profiles(f: &mut Frame, area: Rect, profiles: &mut Profiles, icons: bool) {
    if profiles.rows.is_empty() {
        let message = profiles.error.as_deref().unwrap_or("Loading profiles…");
        f.render_widget(Paragraph::new(Line::styled(message, dim())), area);
        return;
    }
    let show_flavor = area.width >= 78;
    let show_tags = area.width >= 101;
    let mut header = vec!["PROFILE", "ARCH", "VCPU", "RAM (GB)", "DISK (GB)"];
    let mut widths = vec![
        Constraint::Fill(1),
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(8),
        Constraint::Length(9),
    ];
    if show_flavor {
        header.push("FLAVOR / RUNNER");
        widths.push(Constraint::Length(20));
    }
    if show_tags {
        header.push("IMAGE TAGS");
        widths.push(Constraint::Length(24));
    }
    let rows = profiles.rows.iter().map(|profile| {
        let name = profile["name"].as_str().unwrap_or("-");
        let mut label = Line::default();
        if icons {
            let glyph = crate::pool_icon::profile_glyph(profile);
            if glyph == crate::pool_icon::GITLAB {
                label.push_span(Span::styled(glyph, provider_icon_style(glyph)));
            } else {
                label.push_span(glyph);
            }
            label.push_span(" ");
        }
        label.push_span(Span::styled(name, Style::new().fg(ACCENT)));
        if profile["default"] == true {
            label.push_span(Span::styled("*", Style::new().fg(Color::Green)));
        }
        let [ram, disk] = crate::table::profile_sizes(profile);
        let vcpus = profile["vcpus"]
            .as_u64()
            .map_or_else(|| "-".into(), |n| n.to_string());
        let mut cells = vec![
            Cell::from(label),
            Cell::from(profile["arch"].as_str().unwrap_or("-")),
            Cell::from(vcpus),
            Cell::from(ram),
            Cell::from(disk),
        ];
        if show_flavor {
            cells.push(Cell::from(profile["flavor"].as_str().unwrap_or("-")));
        }
        if show_tags {
            let tags = profile["image_tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            cells.push(Cell::from(tags));
        }
        Row::new(cells)
    });
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().add_modifier(Modifier::BOLD | Modifier::DIM)))
        .column_spacing(2)
        .row_highlight_style(
            Style::new()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(table, area, &mut profiles.state);
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
            "No active jobs."
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

fn draw_text(f: &mut Frame, area: Rect, v: &mut TextView, plain: bool) {
    let inner = if plain {
        area
    } else {
        let mut title = vec![Span::styled(format!(" {} ", v.title), Style::new().bold())];
        match &v.exit {
            None | Some(Some(0)) => {}
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
        inner
    };
    draw_text_content(f, inner, v, plain);
}

fn draw_text_content(f: &mut Frame, inner: Rect, v: &mut TextView, plain: bool) {
    let height = inner.height as usize;
    v.height = height;
    let needle = v.filter.to_lowercase();
    let shown: Vec<_> = v
        .lines
        .iter()
        .map(|(line, err)| {
            let line = match &v.trim {
                Some((re, rep)) if !plain => re.replace(line, *rep),
                _ => std::borrow::Cow::Borrowed(line.as_str()),
            };
            (line, *err)
        })
        .filter(|(l, _)| needle.is_empty() || ansi::strip(l).to_lowercase().contains(&needle))
        .collect();
    if shown.is_empty() {
        if plain {
            return;
        }
        let msg = if v.no_jobs && v.filter.is_empty() {
            "⚠ No jobs on this worker yet"
        } else if v.exit.is_none() {
            "waiting for output…"
        } else {
            "no output"
        };
        let style = if v.no_jobs && v.filter.is_empty() {
            Style::new().fg(Color::Yellow)
        } else {
            dim()
        };
        f.render_widget(Paragraph::new(Line::styled(msg, style)), inner);
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
    if v.follow {
        v.top = max_top;
    } else {
        v.top = v.top.min(max_top);
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
        let (table, WorkerPane::Sidebar(sidebar), _) =
            workers_layout(area, Rect::default(), true, None, None)
        else {
            panic!("expected sidebar on wide terminal");
        };
        assert_eq!(sidebar.x, table.right());
        assert_eq!(sidebar.height, table.height);

        let area = Rect::new(0, 1, 72, 24);
        let (table, WorkerPane::Below(details), _) =
            workers_layout(area, Rect::default(), true, None, None)
        else {
            panic!("expected details under the table");
        };
        assert_eq!(details.y, table.bottom());
        assert_eq!(details.width, table.width);

        let area = Rect::new(0, 1, 72, 10);
        assert!(
            matches!(workers_layout(area, Rect::default(), true, None, None), (table, WorkerPane::Hidden, _) if table == area)
        );
        let area = Rect::new(0, 1, 160, 28);
        assert!(
            matches!(workers_layout(area, Rect::default(), false, None, None), (table, WorkerPane::Hidden, _) if table == area)
        );
    }

    #[test]
    fn status_sits_under_the_table_when_logs_are_beside_it() {
        for width in [120, 160] {
            let body = Rect::new(2, 3, width, 20);
            let status = Rect::new(body.x, body.bottom(), body.width, 1);
            let (table, WorkerPane::Sidebar(logs), status) =
                workers_layout(body, status, true, Some(42), None)
            else {
                panic!("expected sidebar");
            };
            assert_eq!(status, Rect::new(table.x, table.bottom(), table.width, 1));
            assert_eq!(logs.x, status.right());
            assert_eq!(logs.bottom(), status.bottom());
        }
        let body = Rect::new(2, 3, 72, 20);
        let status = Rect::new(body.x, body.bottom(), body.width, 1);
        let (_, WorkerPane::Below(logs), footer) = workers_layout(body, status, true, None, None)
        else {
            panic!("expected stacked logs");
        };
        assert_eq!(footer, status);
        assert_eq!(logs.bottom(), footer.y);
    }

    #[test]
    fn log_pane_sizes_clamp_without_crowding_out_workers() {
        let area = Rect::new(0, 1, 120, 28);
        let (table, WorkerPane::Sidebar(logs), _) =
            workers_layout(area, Rect::default(), true, Some(100), Some(4))
        else {
            panic!("expected sidebar");
        };
        assert_eq!((table.width, logs.width), (36, 84));

        let area = Rect::new(0, 1, 72, 24);
        let (table, WorkerPane::Below(logs), _) =
            workers_layout(area, Rect::default(), true, Some(100), Some(22))
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
        let circus = agent_cell("circus", true);
        assert_eq!(circus.spans[0].content, "\u{f0508}");
        assert_eq!(circus.spans.last().unwrap().content, "circus");
        let nix_build = agent_cell("nix build", true);
        assert_eq!(nix_build.spans[0].content, glyph("nixbot"));
        assert_eq!(nix_build.spans[0].style.fg, Some(Color::Rgb(126, 186, 228)));
        assert_eq!(nix_build.spans.last().unwrap().content, "nix build");
    }
}
