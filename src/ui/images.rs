use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bollard::models::ImageSummary;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use chrono::{TimeZone, Utc};

use crate::ui::containers;
use crate::ui::pull::{PullPopup, spawn_image_pull};
use crate::{docker::DockerClient, theme::Theme};
use containers::util::{grad_sweep, truncate_middle};

/// Sort keys available for images
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Repo,
    Size,
    Created,
}

/// Popups used in the Images tab
enum Popup {
    ConfirmDelete { id: String, name: String },
    Inspect(String),
}

pub struct ImagesView {
    docker: DockerClient,
    theme: Theme,
    rows: Vec<ImageSummary>,
    state: TableState,
    pub last_note: Option<(String, Color)>,

    last_refresh: Instant,
    tick: u64,

    // search / filter
    searching: bool,
    query: String,
    dangling_only: bool,

    // sort
    sort_key: SortKey,
    sort_asc: bool,

    // multi-select
    selected_ids: HashSet<String>,

    popup: Option<Popup>,
    pull: PullPopup,
}

impl ImagesView {
    pub async fn new(docker: DockerClient, theme: Theme) -> Result<Self> {
        let mut s = Self {
            docker,
            theme,
            rows: Vec::new(),
            state: TableState::default(),
            last_note: None,
            last_refresh: Instant::now(),
            tick: 0,
            searching: false,
            query: String::new(),
            dangling_only: false,
            sort_key: SortKey::Repo,
            sort_asc: true,
            selected_ids: HashSet::new(),
            popup: None,
            pull: PullPopup::new(),
        };
        s.refresh().await?;
        Ok(s)
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.pull.on_tick();
        if let Some(ok) = self.pull.take_finished() {
            // New layers/tags may now exist locally: refresh the listing.
            let _ = futures_lite::future::block_on(self.refresh());
            if ok {
                self.note_ok("✅ pull complete");
            } else {
                self.note_err("❌ pull finished with errors");
            }
        }
    }

    pub fn has_modal(&self) -> bool {
        self.popup.is_some() || self.pull.visible
    }

    pub fn is_modal_open(&self) -> bool {
        self.has_modal() || self.searching
    }

    async fn refresh(&mut self) -> Result<()> {
        // true => list all images
        self.rows = self.docker.list_images(true).await?;

        // realign selection on filtered view
        let vis_len = self.visible_indices().len();
        if self.state.selected().unwrap_or(0) >= vis_len {
            let len = vis_len.saturating_sub(1);
            self.state
                .select(if vis_len == 0 { None } else { Some(len) });
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    pub async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // 0) pull popup open -> it owns the keyboard
        if self.pull.visible {
            self.pull.on_key(key);
            return Ok(());
        }

        // 1) popup open -> it has priority
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
                        self.delete_image(&id, &name).await?;
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

            // realign selection
            let vis = self.visible_indices();
            let vis_len = vis.len();
            let cur = self.state.selected().unwrap_or(0);
            if cur >= vis_len {
                self.state.select(if vis_len == 0 {
                    None
                } else {
                    Some(vis_len - 1)
                });
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
                self.note_ok("✅ images refreshed");
            }

            // toggle dangling
            KeyCode::Char('a') => {
                self.dangling_only = !self.dangling_only;
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

            // inspect
            KeyCode::Char('i') => {
                if let Some((id, _name)) = self.current_id_and_name() {
                    match self.docker.inspect_image(&id).await {
                        Ok(ins) => {
                            let txt = match serde_json::to_string_pretty(&ins) {
                                Ok(s) => s,
                                Err(_) => format!("{:#?}", ins),
                            };
                            self.popup = Some(Popup::Inspect(txt));
                        }
                        Err(e) => self.note_err(format!("❌ inspect image: {e}")),
                    }
                } else {
                    self.note_warn("⚠ no image selected");
                }
            }

            // delete
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some((id, name)) = self.current_id_and_name() {
                    self.popup = Some(Popup::ConfirmDelete { id, name });
                } else {
                    self.note_warn("⚠ no image selected");
                }
            }

            // multi-select
            KeyCode::Char('x') => {
                if let Some((id, _name)) = self.current_id_and_name() {
                    if !self.selected_ids.remove(&id) {
                        self.selected_ids.insert(id);
                    }
                } else {
                    self.note_warn("⚠ no image selected");
                }
            }
            KeyCode::Char('C') => {
                self.selected_ids.clear();
            }

            // pull (selection, or current row when nothing is selected)
            KeyCode::Char('p') => {
                let targets = self.pull_targets();
                if targets.is_empty() {
                    self.note_warn("⚠ nothing to pull (need a repo:tag)");
                } else {
                    let title = if targets.len() == 1 {
                        format!("Pull {}", targets[0])
                    } else {
                        format!("Pull {} images", targets.len())
                    };
                    let (rx, handle) = spawn_image_pull(self.docker.clone(), targets);
                    self.pull.start(title, rx, handle);
                    self.note_ok("⏬ pulling…");
                }
            }

            // export visible list
            KeyCode::Char('S') => match self.save_visible_to_tmp() {
                Ok(p) => self.note_ok(format!("💾 images saved: {}", p.display())),
                Err(e) => self.note_err(format!("❌ save images: {e}")),
            },

            _ => {}
        }

        Ok(())
    }

    async fn delete_image(&mut self, id: &str, name: &str) -> Result<()> {
        match self.docker.remove_image(id, true, false).await {
            Ok(_) => {
                self.note_ok(format!("🗑 deleted: {name}"));
                let _ = self.refresh().await;
            }
            Err(e) => {
                self.note_err(format!("❌ delete image: {e}"));
            }
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
        let img = self.rows.get(row_idx)?;

        let id = img.id.clone();
        let name = image_name(img);
        Some((id, name))
    }

    fn pull_targets(&self) -> Vec<String> {
        if !self.selected_ids.is_empty() {
            self.rows
                .iter()
                .filter(|img| self.selected_ids.contains(&img.id))
                .filter_map(pullable_ref)
                .collect()
        } else {
            self.state
                .selected()
                .and_then(|idx| self.visible_indices().get(idx).copied())
                .and_then(|row_idx| self.rows.get(row_idx))
                .and_then(pullable_ref)
                .into_iter()
                .collect()
        }
    }

    fn cycle_sort(&mut self) {
        self.sort_key = match self.sort_key {
            SortKey::Repo => SortKey::Size,
            SortKey::Size => SortKey::Created,
            SortKey::Created => SortKey::Repo,
        };
    }

    /// Filtered + sorted row indices
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
            .filter(|(_, img)| match_visible(img, &tokens, self.dangling_only))
            .map(|(i, _)| i)
            .collect();

        indices.sort_by(|&a, &b| {
            let ia = &self.rows[a];
            let ib = &self.rows[b];
            let ord = match self.sort_key {
                SortKey::Repo => key_repo(ia).cmp(&key_repo(ib)),
                SortKey::Size => key_size(ia).cmp(&key_size(ib)),
                SortKey::Created => key_created(ia).cmp(&key_created(ib)),
            };
            if self.sort_asc { ord } else { ord.reverse() }
        });

        indices
    }

    fn save_visible_to_tmp(&self) -> Result<PathBuf> {
        let mut path = std::env::temp_dir();
        path.push("dockrtui_images.txt");

        let vis = self.visible_indices();
        let mut out = String::new();
        out.push_str("REPO:TAG\tSIZE\tCREATED\tID\n");
        for idx in vis {
            let img = &self.rows[idx];
            let name = image_name(img);
            let size = human_size(img.size);
            let created = format_created_full(img.created);
            let id_short = truncate_middle(&img.id, 20);
            out.push_str(&format!("{name}\t{size}\t{created}\t{id_short}\n"));
        }
        fs::write(&path, out)?;
        Ok(path)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.theme;

        let pulse = ((self.tick % 60) as f32 / 60.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        let _pulse = pulse; // potentially useful later
        let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = spinners[(self.tick as usize) % spinners.len()];
        let just_refreshed = self.last_refresh.elapsed() < Duration::from_millis(800);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        // Header / hints
        let phase = (self.tick % 120) as f32 / 120.0;
        let title_line = grad_sweep(" Images ", theme.accent, theme.accent_alt, phase);

        let sort_name = match self.sort_key {
            SortKey::Repo => "repo",
            SortKey::Size => "size",
            SortKey::Created => "created",
        };
        let arrow = if self.sort_asc { "↑" } else { "↓" };

        let mode = if self.dangling_only {
            "dangling"
        } else {
            "all"
        };

        let mut spans = vec![Span::raw(" ")];
        spans.extend(title_line.spans.clone());
        spans.push(Span::raw(
            "  j/k ↑/↓ • /: search • a: all/dangling • o/O: sort • r/F5: refresh • i: inspect • d: delete • x: select • p: pull • S: save",
        ));

        if !self.selected_ids.is_empty() {
            spans.push(Span::styled(
                format!(" | selected: {}", self.selected_ids.len()),
                Style::default().fg(theme.accent),
            ));
        }
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
            spans.push(Span::styled(
                format!(" {spin}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(spans)).block(theme.block("Images"));
        f.render_widget(header, layout[0]);

        // Table
        let vis = self.visible_indices();
        let selected_row = self.state.selected().unwrap_or(0);

        let header_row = Row::new(vec!["REPO:TAG", "SIZE", "CREATED", "ID"]).style(
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        );

        let rows = vis.iter().enumerate().map(|(i, &idx)| {
            let img = &self.rows[idx];

            let checkbox = if self.selected_ids.contains(&img.id) {
                "▣ "
            } else {
                "▢ "
            };
            let name = format!("{checkbox}{}", image_name(img));
            let size_txt = human_size(img.size);
            let created_txt = format_created_full(img.created);
            let id_short = truncate_middle(&img.id, 20);

            let mut row = Row::new(vec![
                Cell::from(name),
                Cell::from(size_txt),
                Cell::from(created_txt),
                Cell::from(id_short),
            ]);

            if i == selected_row {
                // subtle highlight for selected row
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
            Constraint::Percentage(50),
            Constraint::Length(12),
            Constraint::Length(22),
            Constraint::Length(24),
        ];

        let table = Table::new(rows, widths)
            .header(header_row)
            .column_spacing(2)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.muted))
                    .title(theme.title("Images")),
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
                .title(self.theme.title("Inspect image (esc to close)"))
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
                .title(self.theme.title("Delete image? (y/n/esc)"))
                .border_style(Style::default().fg(self.theme.err));
            f.render_widget(block, overlay);

            let msg = format!("Confirm deletion of image `{name}` ?");
            let para = Paragraph::new(Text::raw(msg)).wrap(Wrap { trim: false });
            f.render_widget(para, inner);
        }

        // PULL PROGRESS (drawn last so it sits on top)
        self.pull.draw(f, area, self.theme, self.tick);
    }
}

/* ================= helpers: filter / sort / format ================= */

fn image_name(img: &ImageSummary) -> String {
    if !img.repo_tags.is_empty() {
        return truncate_middle(&img.repo_tags[0], 42);
    }
    "<none>:<none>".into()
}

fn pullable_ref(img: &ImageSummary) -> Option<String> {
    img.repo_tags
        .iter()
        .find(|t| !t.is_empty() && !t.starts_with("<none>"))
        .cloned()
}

fn match_visible(img: &ImageSummary, tokens: &[String], dangling_only: bool) -> bool {
    if dangling_only {
        let dangling =
            img.repo_tags.is_empty() || img.repo_tags.iter().all(|t| t.starts_with("<none>"));
        if !dangling {
            return false;
        }
    }

    if tokens.is_empty() {
        return true;
    }

    let name = img.repo_tags.join(" ").to_lowercase();
    let id = img.id.to_lowercase();

    tokens.iter().all(|t| name.contains(t) || id.contains(t))
}

fn key_repo(img: &ImageSummary) -> String {
    image_name(img).to_lowercase()
}
fn key_size(img: &ImageSummary) -> i64 {
    img.size
}
fn key_created(img: &ImageSummary) -> i64 {
    img.created
}

fn human_size(bytes: i64) -> String {
    let mut v = bytes.max(0) as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{:.0} {}", v, units[i])
    } else {
        format!("{:.1} {}", v, units[i])
    }
}

fn format_age(created: i64) -> String {
    if created <= 0 {
        return "-".into();
    }
    let ts = created as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(ts);

    let days = diff / 86_400;
    let hours = (diff % 86_400) / 3_600;
    let mins = (diff % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn format_created_full(created: i64) -> String {
    if created <= 0 {
        return "-".into();
    }

    let age = format_age(created);
    let ts = created;

    let dt_opt = Utc.timestamp_opt(ts, 0).single();
    if let Some(dt) = dt_opt {
        let date_str = dt.format("%Y-%m-%d").to_string();
        format!("{date_str} ({age})")
    } else {
        age
    }
}
