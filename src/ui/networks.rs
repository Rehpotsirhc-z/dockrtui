use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::{docker::DockerClient, theme::Theme};
use crate::ui::containers;
use containers::util::{grad_sweep, truncate_middle};

/// Available sort keys for networks
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Name,
    Driver,
    Scope,
}

/// Popups for the networks tab
enum Popup {
    ConfirmDelete { id: String, name: String },
    Inspect(String),
}

#[derive(Clone, Debug)]
struct NetworkRow {
    id: String,
    name: String,
    driver: String,
    scope: String,
}

pub struct NetworksView {
    _docker: DockerClient, // kept for symmetry, unused for now (we call the CLI)
    theme: Theme,

    rows: Vec<NetworkRow>,
    state: TableState,
    pub last_note: Option<(String, Color)>,

    last_refresh: Instant,
    tick: u64,

    // search / filter
    searching: bool,
    query: String,
    show_builtin: bool, // bridge / host / none

    // sorting
    sort_key: SortKey,
    sort_asc: bool,

    popup: Option<Popup>,
}

impl NetworksView {
    pub async fn new(docker: DockerClient, theme: Theme) -> Result<Self> {
        let mut s = Self {
            _docker: docker,
            theme,
            rows: Vec::new(),
            state: TableState::default(),
            last_note: None,
            last_refresh: Instant::now(),
            tick: 0,
            searching: false,
            query: String::new(),
            show_builtin: false,
            sort_key: SortKey::Name,
            sort_asc: true,
            popup: None,
        };
        s.refresh().await?;
        Ok(s)
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn is_modal_open(&self) -> bool {
        self.popup.is_some() || self.searching
    }

    /// Match Ui API (like ContainersView)
    pub fn has_modal(&self) -> bool {
        self.is_modal_open()
    }

    async fn refresh(&mut self) -> Result<()> {
        self.rows = self.fetch_networks()?;

        let vis_len = self.visible_indices().len();
        if self.state.selected().unwrap_or(0) >= vis_len {
            let len = vis_len.saturating_sub(1);
            self.state.select(if vis_len == 0 { None } else { Some(len) });
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn fetch_networks(&self) -> Result<Vec<NetworkRow>> {
        let output = Command::new("docker")
            .args([
                "network",
                "ls",
                "--format",
                "{{.ID}}\t{{.Name}}\t{{.Driver}}\t{{.Scope}}",
            ])
            .output()?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("docker network ls failed: {err}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 4 {
                continue;
            }
            out.push(NetworkRow {
                id: parts[0].to_string(),
                name: parts[1].to_string(),
                driver: parts[2].to_string(),
                scope: parts[3].to_string(),
            });
        }

        Ok(out)
    }

    pub async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // 1) popup visible
        if let Some(p) = &mut self.popup {
            match p {
                Popup::ConfirmDelete { .. } => match key.code {
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.popup = None;
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        let (id, name) =
                            if let Some(Popup::ConfirmDelete { id, name }) = self.popup.take() {
                                (id, name)
                            } else {
                                return Ok(());
                            };
                        self.delete_network(&id, &name).await?;
                    }
                    _ => {}
                },
                Popup::Inspect(_) => match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.popup = None;
                    }
                    _ => {}
                },
            }
            return Ok(());
        }

        // 2) search mode
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                }
                KeyCode::Enter => {
                    self.searching = false;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(ch) => {
                    self.query.push(ch);
                }
                _ => {}
            }

            let vis = self.visible_indices();
            let vis_len = vis.len();
            let cur = self.state.selected().unwrap_or(0);
            if cur >= vis_len {
                self.state
                    .select(if vis_len == 0 { None } else { Some(vis_len - 1) });
            }
            return Ok(());
        }

        // 3) normal shortcuts
        match key.code {
            // navigation
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),

            // refresh
            KeyCode::Char('r') | KeyCode::F(5) => {
                self.refresh().await?;
                self.note_ok("✅ networks refreshed");
            }

            // toggle built-in networks
            KeyCode::Char('a') => {
                self.show_builtin = !self.show_builtin;
            }

            // search
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }

            // sort
            KeyCode::Char('o') => self.cycle_sort(),
            KeyCode::Char('O') => {
                self.sort_asc = !self.sort_asc;
            }

            // inspect (docker network inspect)
            KeyCode::Char('i') => {
                if let Some((id, _name)) = self.current_id_and_name() {
                    match self.inspect_network_cli(&id) {
                        Ok(txt) => {
                            self.popup = Some(Popup::Inspect(txt));
                        }
                        Err(e) => self.note_err(format!("❌ inspect network: {e}")),
                    }
                } else {
                    self.note_warn("⚠ no network selected");
                }
            }

            // delete (only user-defined)
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some((id, name)) = self.current_id_and_name() {
                    if is_builtin_network_name(&name) {
                        self.note_warn("⚠ built-in network cannot be removed (bridge/host/none)");
                    } else {
                        self.popup = Some(Popup::ConfirmDelete { id, name });
                    }
                } else {
                    self.note_warn("⚠ no network selected");
                }
            }

            _ => {}
        }

        Ok(())
    }

    fn inspect_network_cli(&self, id: &str) -> Result<String> {
        let output = Command::new("docker")
            .args(["network", "inspect", id])
            .output()?;

        let mut s = String::new();
        if !output.stdout.is_empty() {
            s.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            s.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        if !output.status.success() {
            return Err(anyhow!(
                "docker network inspect failed (code={:?}): {}",
                output.status.code(),
                s
            ));
        }

        Ok(s)
    }

    async fn delete_network(&mut self, id: &str, name: &str) -> Result<()> {
        let output = Command::new("docker")
            .args(["network", "rm", id])
            .output()?;

        let mut s = String::new();
        if !output.stdout.is_empty() {
            s.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            s.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        if output.status.success() {
            self.note_ok(format!("🗑 removed: {name}"));
            let _ = self.refresh().await;
        } else {
            self.note_err(format!(
                "❌ network removal failed (code={:?}): {}",
                output.status.code(),
                s
            ));
        }
        Ok(())
    }

    fn note_ok<S: Into<String>>(&mut self, msg: S) {
        self.last_note = Some((msg.into(), self.theme.ok));
    }
    fn note_warn<S: Into<String>>(&mut self, msg: S) {
        self.last_note = Some((msg.into(), self.theme.warn));
    }
    fn note_err<S: Into<String>>(&mut self, msg: S) {
        self.last_note = Some((msg.into(), self.theme.err));
    }

    fn move_sel(&mut self, delta: i32) {
        let vis = self.visible_indices();
        let len = vis.len();
        if len == 0 {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
        self.state.select(Some(next));
    }

    fn current_id_and_name(&self) -> Option<(String, String)> {
        let idx = self.state.selected()?;
        let vis = self.visible_indices();
        let row_idx = *vis.get(idx)?;
        let nw = self.rows.get(row_idx)?;

        Some((nw.id.clone(), nw.name.clone()))
    }

    fn cycle_sort(&mut self) {
        self.sort_key = match self.sort_key {
            SortKey::Name => SortKey::Driver,
            SortKey::Driver => SortKey::Scope,
            SortKey::Scope => SortKey::Name,
        };
    }

    /// filtered + sorted indices
    fn visible_indices(&self) -> Vec<usize> {
        let tokens = self
            .query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect::<Vec<_>>();

        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, nw)| match_visible(nw, &tokens, self.show_builtin))
            .map(|(i, _)| i)
            .collect();

        indices.sort_by(|&a, &b| {
            let na = &self.rows[a];
            let nb = &self.rows[b];
            let ord = match self.sort_key {
                SortKey::Name => key_name(na).cmp(&key_name(nb)),
                SortKey::Driver => key_driver(na).cmp(&key_driver(nb)),
                SortKey::Scope => key_scope(na).cmp(&key_scope(nb)),
            };
            if self.sort_asc {
                ord
            } else {
                ord.reverse()
            }
        });

        indices
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.theme;

        let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = spinners[(self.tick as usize) % spinners.len()];
        let just_refreshed = self.last_refresh.elapsed() < Duration::from_millis(800);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        // Header / hints
        let phase = (self.tick % 120) as f32 / 120.0;
        let title_line = grad_sweep(" Networks ", theme.accent, theme.accent_alt, phase);

        let sort_name = match self.sort_key {
            SortKey::Name => "name",
            SortKey::Driver => "driver",
            SortKey::Scope => "scope",
        };
        let arrow = if self.sort_asc { "↑" } else { "↓" };
        let mode = if self.show_builtin { "all" } else { "user-defined" };

        let mut spans = vec![Span::raw(" ")];
        spans.extend(title_line.spans.clone());
        spans.push(Span::raw(
            "  j/k ↑/↓ • /: search • a: all/user • o/O: sort • r/F5: refresh • i: inspect • d: delete",
        ));

        if !self.query.is_empty() {
            spans.push(Span::styled(
                format!(" | filter: '{}'", self.query),
                Style::default().fg(theme.accent),
            ));
        }
        spans.push(Span::styled(
            format!(" | sort: {sort_name}{arrow}"),
            Style::default().fg(theme.muted),
        ));
        spans.push(Span::styled(
            format!(" | mode: {mode}"),
            Style::default().fg(theme.muted),
        ));
        if just_refreshed {
            spans.push(
                Span::styled(
                    format!(" {spin}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            );
        }

        let header = Paragraph::new(Line::from(spans)).block(theme.block("Networks"));
        f.render_widget(header, layout[0]);

        // Table
        let vis = self.visible_indices();
        let selected_row = self.state.selected().unwrap_or(0);

        let header_row = Row::new(vec!["NAME", "DRIVER", "SCOPE", "ATTACHED", "ID"])
            .style(Style::default().fg(theme.muted).add_modifier(Modifier::BOLD));

        let rows = vis.iter().enumerate().map(|(i, &idx)| {
            let nw = &self.rows[idx];

            let name = nw.name.clone();
            let driver = nw.driver.clone();
            let scope = nw.scope.clone();
            let attached = "-".to_string(); // v1: we don't compute attached containers
            let id_short = truncate_middle(&nw.id, 18);

            let mut row = Row::new(vec![
                Cell::from(name),
                Cell::from(driver),
                Cell::from(scope),
                Cell::from(attached),
                Cell::from(id_short),
            ]);

            if i == selected_row {
                row = row.style(
                    Style::default()
                        .bg(Color::Rgb(24, 26, 30))
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                );
            } else if i % 2 == 1 {
                row = row.style(Style::default().bg(Color::Rgb(16, 18, 20)));
            }

            row
        });

        let widths = [
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(22),
        ];

        let table = Table::new(rows, widths)
            .header(header_row)
            .column_spacing(2)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.muted))
                    .title(theme.title("Networks")),
            )
            .highlight_symbol("❯ ");

        f.render_stateful_widget(table, layout[1], &mut self.state);

        // POPUPS
        if let Some(Popup::Inspect(txt)) = &self.popup {
            let w = (area.width * 3 / 4).max(40);
            let h = (area.height * 3 / 4).max(10);
            let overlay = Rect {
                x: area.x + (area.width - w) / 2,
                y: area.y + (area.height - h) / 2,
                width: w,
                height: h,
            };
            f.render_widget(Clear, overlay);
            let inner = Rect {
                x: overlay.x + 1,
                y: overlay.y + 1,
                width: overlay.width - 2,
                height: overlay.height - 2,
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(self.theme.title("Inspect network (esc to close)"))
                .border_style(Style::default().fg(self.theme.accent));
            f.render_widget(block, overlay);

            let para = Paragraph::new(Text::raw(txt.clone())).wrap(Wrap { trim: false });
            f.render_widget(para, inner);
        }

        if let Some(Popup::ConfirmDelete { id: _, name }) = &self.popup {
            let w = (area.width / 2).max(48);
            let h = 7u16;
            let overlay = Rect {
                x: area.x + (area.width - w) / 2,
                y: area.y + (area.height - h) / 2,
                width: w,
                height: h,
            };
            f.render_widget(Clear, overlay);
            let inner = Rect {
                x: overlay.x + 1,
                y: overlay.y + 1,
                width: overlay.width - 2,
                height: overlay.height - 2,
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(self.theme.title("Delete network? (y/n/esc)"))
                .border_style(Style::default().fg(self.theme.err));
            f.render_widget(block, overlay);

            let msg = format!("Confirm deletion of network `{name}`?");
            let para = Paragraph::new(Text::raw(msg)).wrap(Wrap { trim: false });
            f.render_widget(para, inner);
        }
    }
}

/* ================= helpers ================= */

fn is_builtin_network_name(name: &str) -> bool {
    matches!(name, "bridge" | "host" | "none")
}

fn match_visible(nw: &NetworkRow, tokens: &[String], show_builtin: bool) -> bool {
    if !show_builtin && is_builtin_network_name(&nw.name) {
        return false;
    }

    if tokens.is_empty() {
        return true;
    }

    let hay = format!(
        "{} {} {} {}",
        nw.name.to_lowercase(),
        nw.id.to_lowercase(),
        nw.driver.to_lowercase(),
        nw.scope.to_lowercase()
    );

    tokens.iter().all(|t| hay.contains(t))
}

fn key_name(nw: &NetworkRow) -> String {
    nw.name.to_lowercase()
}
fn key_driver(nw: &NetworkRow) -> String {
    nw.driver.to_lowercase()
}
fn key_scope(nw: &NetworkRow) -> String {
    nw.scope.to_lowercase()
}
