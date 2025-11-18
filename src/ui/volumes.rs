use std::time::{Instant};

use anyhow::Result;
use bollard::models::Volume;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::ui::containers;
use crate::{docker::DockerClient, theme::Theme};
use containers::util::{ truncate_middle};

/// Sort keys available for volumes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Name,
    Driver,
    Mountpoint,
    CreatedAt,
}

/// Popups used in the Volumes tab
enum Popup {
    ConfirmDelete { name: String },
    ConfirmPrune,
    Inspect(String),
}

pub struct VolumesView {
    docker: DockerClient,
    theme: Theme,
    rows: Vec<Volume>,
    state: TableState,
    pub last_note: Option<(String, Color)>,

    last_refresh: Instant,
    tick: u64,

    // search / filter
    searching: bool,
    query: String,

    // sort
    sort_key: SortKey,
    sort_asc: bool,

    popup: Option<Popup>,
}

impl VolumesView {
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

    pub fn has_modal(&self) -> bool {
        self.is_modal_open()
    }

    async fn refresh(&mut self) -> Result<()> {
        self.rows = self.docker.list_volumes().await?;

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
        // 1) popup open -> it has priority
        if let Some(p) = &mut self.popup {
            match p {
                Popup::ConfirmDelete { .. } => match key.code {
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.popup = None;
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        let name = if let Some(Popup::ConfirmDelete { name }) = self.popup.take()
                        {
                            name
                        } else {
                            return Ok(());
                        };
                        self.delete_volume(&name).await?;
                    }
                    _ => {}
                },
                Popup::ConfirmPrune => match key.code {
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.popup = None;
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        self.popup = None;
                        self.prune_volumes().await?;
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
                self.note_ok("✅ volumes refreshed");
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
                if let Some(name) = self.current_name() {
                    match self.docker.inspect_volume(&name).await {
                        Ok(ins) => {
                            let txt = match serde_json::to_string_pretty(&ins) {
                                Ok(s) => s,
                                Err(_) => format!("{:#?}", ins),
                            };
                            self.popup = Some(Popup::Inspect(txt));
                        }
                        Err(e) => self.note_err(format!("❌ inspect volume: {e}")),
                    }
                }
            }

            // delete
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(name) = self.current_name() {
                    self.popup = Some(Popup::ConfirmDelete { name });
                }
            }

            // prune
            KeyCode::Char('p') => {
                self.popup = Some(Popup::ConfirmPrune);
            }

            _ => {}
        }
        Ok(())
    }

    async fn delete_volume(&mut self, name: &str) -> Result<()> {
        match self.docker.remove_volume(name, false).await {
            Ok(_) => {
                self.note_ok(format!("🗑 deleted: {name}"));
                let _ = self.refresh().await;
            }
            Err(e) => {
                self.note_err(format!("❌ delete failed: {e}"));
            }
        }
        Ok(())
    }

    async fn prune_volumes(&mut self) -> Result<()> {
        match self.docker.prune_volumes().await {
            Ok(_) => {
                self.note_ok("🧹 unused volumes pruned");
                let _ = self.refresh().await;
            }
            Err(e) => {
                self.note_err(format!("❌ prune failed: {e}"));
            }
        }
        Ok(())
    }

    fn move_sel(&mut self, delta: i32) {
        let vis = self.visible_indices();
        let len = vis.len();
        if len == 0 {
            return;
        }

        let cur = self.state.selected().unwrap_or(0);
        let new = if delta > 0 {
            (cur + 1).min(len - 1)
        } else if cur > 0 {
            cur - 1
        } else {
            0
        };
        self.state.select(Some(new));
    }

    fn cycle_sort(&mut self) {
        self.sort_key = match self.sort_key {
            SortKey::Name => SortKey::Driver,
            SortKey::Driver => SortKey::Mountpoint,
            SortKey::Mountpoint => SortKey::CreatedAt,
            SortKey::CreatedAt => SortKey::Name,
        };
    }

    fn visible_indices(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_i, vol)| {
                if q.is_empty() {
                    return true;
                }
                vol.name.to_lowercase().contains(&q)
                    || vol.driver.to_lowercase().contains(&q)
                    || vol.mountpoint.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn current_name(&self) -> Option<String> {
        let vis = self.visible_indices();
        let sel = self.state.selected()?;
        if sel >= vis.len() {
            return None;
        }
        let idx = vis[sel];
        Some(self.rows[idx].name.clone())
    }

    fn note_ok(&mut self, msg: impl Into<String>) {
        self.last_note = Some((msg.into(), self.theme.ok));
    }

    fn note_err(&mut self, msg: impl Into<String>) {
        self.last_note = Some((msg.into(), self.theme.err));
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let vis = self.visible_indices();

        // Apply sorting
        let mut sorted_vis = vis.clone();
        sorted_vis.sort_by(|&a, &b| {
            let vol_a = &self.rows[a];
            let vol_b = &self.rows[b];

            let cmp = match self.sort_key {
                SortKey::Name => vol_a.name.cmp(&vol_b.name),
                SortKey::Driver => vol_a.driver.cmp(&vol_b.driver),
                SortKey::Mountpoint => vol_a.mountpoint.cmp(&vol_b.mountpoint),
                SortKey::CreatedAt => vol_a.created_at.cmp(&vol_b.created_at),
            };

            if self.sort_asc {
                cmp
            } else {
                cmp.reverse()
            }
        });

        // Build table rows
        let header_cells = ["Name", "Driver", "Mountpoint", "Scope"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().fg(self.theme.accent).add_modifier(Modifier::BOLD)));
        let header = Row::new(header_cells).height(1).bottom_margin(1);

        let rows_iter = sorted_vis.iter().map(|&idx| {
            let vol = &self.rows[idx];

            let name_cell = Cell::from(truncate_middle(&vol.name, 30));
            let driver_cell = Cell::from(vol.driver.as_str());
            let mountpoint_cell = Cell::from(truncate_middle(&vol.mountpoint, 40));
            let scope_str = vol.scope.as_ref().map(|s| format!("{:?}", s)).unwrap_or_else(|| "N/A".to_string());
            let scope_cell = Cell::from(scope_str);

            Row::new(vec![name_cell, driver_cell, mountpoint_cell, scope_cell]).height(1)
        });

        let mut title_text = format!(" Volumes ({}) ", self.rows.len());
        if !self.query.is_empty() {
            title_text.push_str(&format!("filtered: {} ", sorted_vis.len()));
        }

        // sort indicator
        let sort_key_str = match self.sort_key {
            SortKey::Name => "name",
            SortKey::Driver => "driver",
            SortKey::Mountpoint => "mountpoint",
            SortKey::CreatedAt => "created",
        };
        let sort_dir = if self.sort_asc { "↑" } else { "↓" };
        title_text.push_str(&format!(" • sort: {}{} ", sort_key_str, sort_dir));

        let table_block = self.theme.block(&title_text);

        let table = Table::new(
            rows_iter,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(15),
                Constraint::Percentage(45),
                Constraint::Percentage(15),
            ],
        )
        .header(header)
        .block(table_block)
        .row_highlight_style(
            Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::REVERSED),
        );

        f.render_stateful_widget(table, area, &mut self.state);

        // Overlays: popups
        if let Some(p) = &self.popup {
            match p {
                Popup::ConfirmDelete { name } => {
                    self.draw_confirm_delete(f, area, name);
                }
                Popup::ConfirmPrune => {
                    self.draw_confirm_prune(f, area);
                }
                Popup::Inspect(txt) => {
                    self.draw_inspect(f, area, txt);
                }
            }
        }

        // Search bar
        if self.searching {
            self.draw_search(f, area);
        }
    }

    fn draw_confirm_delete(&self, f: &mut Frame, area: Rect, name: &str) {
        let w = 60.min(area.width);
        let h = 7;
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup_rect = Rect {
            x: area.x + x,
            y: area.y + y,
            width: w,
            height: h,
        };

        f.render_widget(Clear, popup_rect);

        let block = Block::default()
            .title(self.theme.title(" Confirm Delete "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.warn));

        let text = format!("Delete volume: {}?\n\nPress [y] to confirm, [n] to cancel", name);
        let para = Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(self.theme.fg));

        f.render_widget(para, popup_rect);
    }

    fn draw_confirm_prune(&self, f: &mut Frame, area: Rect) {
        let w = 60.min(area.width);
        let h = 8;
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup_rect = Rect {
            x: area.x + x,
            y: area.y + y,
            width: w,
            height: h,
        };

        f.render_widget(Clear, popup_rect);

        let block = Block::default()
            .title(self.theme.title(" Confirm Prune "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.warn));

        let text = "Prune all unused volumes?\n\nThis will remove volumes not referenced by any container.\n\nPress [y] to confirm, [n] to cancel";
        let para = Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(self.theme.fg));

        f.render_widget(para, popup_rect);
    }

    fn draw_inspect(&self, f: &mut Frame, area: Rect, txt: &str) {
        let w = (area.width * 9 / 10).max(60);
        let h = (area.height * 9 / 10).max(20);
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup_rect = Rect {
            x: area.x + x,
            y: area.y + y,
            width: w,
            height: h,
        };

        f.render_widget(Clear, popup_rect);

        let block = Block::default()
            .title(self.theme.title(" Inspect Volume "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent));

        let para = Paragraph::new(txt)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(self.theme.fg));

        f.render_widget(para, popup_rect);
    }

    fn draw_search(&self, f: &mut Frame, area: Rect) {
        let w = 40.min(area.width);
        let h = 3;
        let x = (area.width.saturating_sub(w)) / 2;
        let y = 2;
        let search_rect = Rect {
            x: area.x + x,
            y: area.y + y,
            width: w,
            height: h,
        };

        f.render_widget(Clear, search_rect);

        let block = Block::default()
            .title(self.theme.title(" Search "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent));

        let content = format!("{}_", self.query);
        let para = Paragraph::new(content)
            .block(block)
            .style(Style::default().fg(self.theme.fg));

        f.render_widget(para, search_rect);
    }
}
