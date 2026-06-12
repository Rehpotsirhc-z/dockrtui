use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use bollard::models::ContainerSummary;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{docker::DockerClient, theme::Theme};

use super::actions::{self, ActionAnim, ActionKind, BarPhase};
use super::progress::fancy_bar_line;
use super::rocket::{render_rocket_scene_vertical, rocket_down_from_bar, rocket_up_from_bar};
use super::shell::{ShellEvent, ShellPopup};
use super::util::{alt_row_style, grad_sweep, lerp, selected_row_style, truncate_middle};

/// Available sort keys
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Name,
    Image,
    State,
    Created, // via .created
    Ports,
}

enum Popup {
    Inspect(String),
    ConfirmDelete { items: Vec<(String, String)> },
}

pub struct ContainersView {
    docker: DockerClient,
    pub all: bool,
    rows: Vec<ContainerSummary>,
    state: TableState,
    pub last_refresh: Instant,
    theme: Theme,
    pub tick: u64,

    // toast to Ui
    pub last_note: Option<(String, Color)>,

    // action overlay (rocket + progress bar) for start/stop
    anim: Option<ActionAnim>,

    // --------- v2 features ----------
    searching: bool,
    query: String,

    // sorting
    sort_key: SortKey,
    sort_asc: bool,

    // multiselect
    selected_ids: HashSet<String>,

    // queued actions (start/stop) (kind, id, name)
    queue: VecDeque<(ActionKind, String, String)>,

    // popups
    popup: Option<Popup>,

    // integrated shell
    shell: Option<ShellPopup>,
}

impl ContainersView {
    pub async fn new(docker: DockerClient, theme: Theme) -> Result<Self> {
        let mut s = Self {
            docker,
            all: true,
            rows: vec![],
            state: TableState::default(),
            last_refresh: Instant::now(),
            theme,
            tick: 0,
            last_note: None,
            anim: None,

            searching: false,
            query: String::new(),
            sort_key: SortKey::Name,
            sort_asc: true,
            selected_ids: HashSet::new(),
            queue: VecDeque::new(),
            popup: None,

            shell: None,
        };
        s.refresh().await?;
        Ok(s)
    }

    pub async fn refresh(&mut self) -> Result<()> {
        self.rows = self.docker.list_containers(self.all).await?;
        // clamp selection to filtered view
        let vis_len = self.visible_indices().len();
        match self.state.selected() {
            Some(sel) if sel >= vis_len => {
                self.state.select(if vis_len == 0 {
                    None
                } else {
                    Some(vis_len - 1)
                });
            }
            None if vis_len > 0 => self.state.select(Some(0)),
            _ => {}
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    pub fn is_modal_open(&self) -> bool {
        self.shell.is_some() || self.popup.is_some() || self.searching
    }

    pub fn has_modal(&self) -> bool {
        self.shell.is_some() || self.popup.is_some()
    }

    pub fn selected_id(&self) -> Option<String> {
        self.current().map(|(id, _name, _state)| id.to_string())
    }

    async fn remove_ids(&mut self, items: Vec<(String, String)>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let mut ok = 0usize;
        let mut ko: Vec<String> = vec![];

        for (id, name) in items {
            // force + volumes
            match self.docker.remove(&id, true, true).await {
                Ok(_) => {
                    ok += 1;
                }
                Err(e) => {
                    ko.push(format!("{name} ({e})"));
                }
            }
        }

        let _ = self.refresh().await;
        if ok > 0 {
            self.note_ok(format!("🗑 removed: {ok}"));
        }
        if !ko.is_empty() {
            self.note_err(format!("❌ failed: {}", ko.join(", ")));
        }
        Ok(())
    }

    pub async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // 0) if shell is open, it has priority
        if let Some(shell) = &mut self.shell {
            match shell.on_key(key)? {
                ShellEvent::None => {}
                ShellEvent::Close => {
                    self.shell = None;
                    self.note_ok("🐚 shell closed");
                }
            }
            return Ok(());
        }

        // 1) popups have priority
        if let Some(p) = &mut self.popup {
            match p {
                Popup::Inspect(_) => match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.popup = None,
                    _ => {}
                },
                Popup::ConfirmDelete { .. } => match key.code {
                    KeyCode::Esc | KeyCode::Char('n') => self.popup = None,
                    KeyCode::Enter | KeyCode::Char('y') => {
                        let items = if let Some(Popup::ConfirmDelete { items }) = self.popup.take()
                        {
                            items
                        } else {
                            vec![]
                        };
                        self.remove_ids(items).await?;
                    }
                    _ => {}
                },
            }
            return Ok(());
        }

        // 2) search input mode
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
            // clamp to new visible set
            let vis_len = self.visible_indices().len();
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

        // 3) shortcuts
        match key.code {
            // navigation
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),

            // all/running
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.refresh().await?;
            }

            // refresh
            KeyCode::Char('r') | KeyCode::F(5) => {
                self.refresh().await?;
                self.note_ok("✅ refreshed");
            }

            // pause/unpause (immediate)
            KeyCode::Char('p') => {
                if let Some((id, name, state)) = self.current() {
                    match state.as_deref() {
                        Some("running") => match self.docker.pause(id).await {
                            Ok(_) => {
                                self.note_ok(format!("⏸ paused {name}"));
                                let _ = self.refresh().await;
                            }
                            Err(e) => self.note_err(format!("❌ pause: {e}")),
                        },
                        Some("paused") => match self.docker.unpause(id).await {
                            Ok(_) => {
                                self.note_ok(format!("▶ unpaused {name}"));
                                let _ = self.refresh().await;
                            }
                            Err(e) => self.note_err(format!("❌ unpause: {e}")),
                        },
                        _ => self.note_warn("⚠ pause/unpause only for running/paused containers"),
                    }
                }
            }

            // Open shell popup
            KeyCode::Char('b') => {
                if let Some((id, name, state)) = self.current() {
                    if !matches!(state.as_deref(), Some("running")) {
                        self.note_warn("⚠ shell only available on a running container");
                    } else {
                        let mut sh = ShellPopup::new(id.to_string(), name.clone(), self.theme);
                        sh.detect_shell();
                        self.shell = Some(sh);
                    }
                } else {
                    self.note_warn("⚠ no container selected");
                }
            }

            // pause/unpause batch
            KeyCode::Char('P') => {
                let items: Vec<(String, String, Option<String>)> = self
                    .visible_indices()
                    .into_iter()
                    .filter_map(|i| self.rows.get(i))
                    .filter_map(|c| {
                        let id = c.id.as_deref()?.to_string();
                        if !self.selected_ids.contains(&id) {
                            return None;
                        }
                        let name = c
                            .names
                            .as_ref()
                            .and_then(|v| v.first())
                            .cloned()
                            .unwrap_or_else(|| id.clone());
                        let state = c.state.as_ref().map(|s| s.to_string());
                        Some((id, name, state))
                    })
                    .collect();

                if items.is_empty() {
                    self.note_warn("⚠ no selection");
                } else {
                    let mut paused = 0usize;
                    let mut unpaused = 0usize;
                    let mut errs: Vec<String> = vec![];

                    for (id, name, st) in items {
                        match st.as_deref() {
                            Some("running") => {
                                if let Err(e) = self.docker.pause(&id).await {
                                    errs.push(format!("{name}: {e}"));
                                } else {
                                    paused += 1;
                                }
                            }
                            Some("paused") => {
                                if let Err(e) = self.docker.unpause(&id).await {
                                    errs.push(format!("{name}: {e}"));
                                } else {
                                    unpaused += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                    let _ = self.refresh().await;
                    if paused > 0 {
                        self.note_ok(format!("⏸ paused: {paused}"));
                    }
                    if unpaused > 0 {
                        self.note_ok(format!("▶ unpaused: {unpaused}"));
                    }
                    if !errs.is_empty() {
                        self.note_err(format!("❌ {}", errs.join(", ")));
                    }
                }
            }

            // search
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }

            // sort
            KeyCode::Char('o') => {
                self.cycle_sort();
            }
            KeyCode::Char('O') => {
                self.sort_asc = !self.sort_asc;
            }

            // multi-select
            KeyCode::Char('x') => {
                if let Some((id, ..)) = self.current() {
                    let id = id.to_string();
                    toggle(&mut self.selected_ids, &id);
                }
            }
            KeyCode::Char('C') => {
                self.selected_ids.clear();
            }
            KeyCode::Char('A') => {
                // Queue start/stop operations based on current state
                let ids: Vec<(String, String, bool)> = self
                    .visible_indices()
                    .into_iter()
                    .filter_map(|idx| self.rows.get(idx))
                    .filter_map(|c| {
                        let id = c.id.as_deref()?.to_string();
                        if !self.selected_ids.contains(&id) {
                            return None;
                        }
                        let name = c
                            .names
                            .as_ref()
                            .and_then(|v| v.first())
                            .cloned()
                            .unwrap_or_else(|| id.clone());
                        let running = matches!(
                            c.state,
                            Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
                        );
                        Some((id, name, running))
                    })
                    .collect();

                if ids.is_empty() {
                    self.note_warn("⚠ no selection");
                } else {
                    for (id, name, running) in ids {
                        let kind = if running {
                            ActionKind::Stopping
                        } else {
                            ActionKind::Starting
                        };
                        self.queue.push_back((kind, id, name));
                    }
                    self.note_ok("🧰 batch queued");
                }
            }

            // ▶ Start/Stop (overlay)
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some((id, name, state)) = self.current() {
                    let kind = if matches!(state.as_deref(), Some("running")) {
                        ActionKind::Stopping
                    } else {
                        ActionKind::Starting
                    };
                    self.queue.push_back((kind, id.to_string(), name));
                } else {
                    self.note_warn("⚠ no container selected");
                }
            }

            // ↻ Restart
            KeyCode::Char('R') => {
                if let Some((id, name, _)) = self.current() {
                    let id = id.to_string();
                    let name = name.clone();
                    self.queue
                        .push_back((ActionKind::Stopping, id.clone(), name.clone()));
                    self.queue.push_back((ActionKind::Starting, id, name));
                } else {
                    self.note_warn("⚠ no container selected");
                }
            }

            // Inspect
            KeyCode::Char('i') => {
                if let Some((id, name, _)) = self.current() {
                    match self.docker.inspect(id).await {
                        Ok(ins) => {
                            let text = format_inspect(&name, &ins);
                            self.popup = Some(Popup::Inspect(text));
                        }
                        Err(e) => self.note_err(format!("❌ inspect: {e}")),
                    }
                }
            }

            // Export
            KeyCode::Char('S') => match self.save_visible_to_tmp() {
                Ok(p) => self.note_ok(format!("💾 saved {}", p.display())),
                Err(e) => self.note_err(format!("❌ save: {e}")),
            },

            // Remove (current)
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some((id, name, _)) = self.current() {
                    self.popup = Some(Popup::ConfirmDelete {
                        items: vec![(id.to_string(), name)],
                    });
                } else {
                    self.note_warn("⚠ no container selected");
                }
            }

            // Remove (batch)
            KeyCode::Char('D') => {
                let items: Vec<(String, String)> = self
                    .visible_indices()
                    .into_iter()
                    .filter_map(|i| self.rows.get(i))
                    .filter_map(|c| {
                        let id = c.id.as_deref()?.to_string();
                        if !self.selected_ids.contains(&id) {
                            return None;
                        }
                        let name = c
                            .names
                            .as_ref()
                            .and_then(|v| v.first())
                            .cloned()
                            .unwrap_or_else(|| id.clone());
                        Some((id, name))
                    })
                    .collect();
                if items.is_empty() {
                    self.note_warn("⚠ no selection");
                } else {
                    self.popup = Some(Popup::ConfirmDelete { items });
                }
            }

            _ => {}
        }
        Ok(())
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        // If no animation is active, start the next queued action (start/stop)
        if self.anim.is_none()
            && let Some((kind, id, name)) = self.queue.pop_front()
            && let Ok(anim) = futures_lite::future::block_on(actions::launch_action(
                self.docker.clone(),
                kind,
                id,
                name,
            ))
        {
            self.anim = Some(anim);
        }

        if let Some(mut anim) = self.anim.take() {
            // Consume updates
            while let Ok(upd) = anim.rx.try_recv() {
                anim.bar_target_pct = upd.pct.clamp(0.0, 1.0);
                anim.bar_phase = upd.phase;
                if matches!(anim.bar_phase, BarPhase::Done | BarPhase::Error)
                    && anim.done_at.is_none()
                {
                    anim.done_at = Some(Instant::now());
                }
            }

            // Smooth progress bar
            let dt = anim.last_bar_tick.elapsed().as_secs_f32();
            anim.last_bar_tick = Instant::now();
            let speed = 0.30; // ~30%/s
            let diff = anim.bar_target_pct - anim.bar_pct;
            let step = speed * dt;
            if diff.abs() <= step {
                anim.bar_pct = anim.bar_target_pct;
            } else {
                anim.bar_pct += step * diff.signum();
            }

            // Rocket synchronized with the bar
            anim.rocket_t = match anim.kind {
                ActionKind::Starting => rocket_up_from_bar(anim.bar_pct),
                ActionKind::Stopping => rocket_down_from_bar(anim.bar_pct),
            };

            // Close when Docker is done AND bar at 100%
            let docker_done =
                anim.done_flag.load(std::sync::atomic::Ordering::Relaxed) && anim.bar_pct >= 0.999;
            let rocket_done = anim.rocket_t >= 0.999;

            if docker_done && rocket_done {
                if let Some(res) = anim.result.lock().unwrap().take() {
                    match (anim.kind, res) {
                        (ActionKind::Starting, Ok(_)) => {
                            self.note_ok(format!("▶ started {}", anim.name))
                        }
                        (ActionKind::Stopping, Ok(_)) => {
                            self.note_ok(format!("⏹ stopped {}", anim.name))
                        }
                        (_, Err(e)) => self.note_err(format!("❌ action {}: {e}", anim.name)),
                    }
                }
                let _ = futures_lite::future::block_on(self.refresh());
            } else {
                self.anim = Some(anim);
            }
        }
    }

    fn move_sel(&mut self, delta: i32) {
        let vis_len = self.visible_indices().len();
        if vis_len == 0 {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, (vis_len - 1) as i32) as usize;
        self.state.select(Some(next));
    }

    /// Id, name, state based on filtered/sorted view
    fn current(&self) -> Option<(&str, String, Option<String>)> {
        let idx = self.state.selected()?;
        let vis = self.visible_indices();
        let row_idx = *vis.get(idx)?;
        let row = self.rows.get(row_idx)?;
        let id = row.id.as_deref()?;
        let name = row
            .names
            .as_ref()
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| id.to_string());
        let state = row.state.as_ref().map(|s| s.to_string());
        Some((id, name, state))
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

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.theme;

        // Animation helpers
        let pulse = ((self.tick % 60) as f32 / 60.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = spinners[(self.tick as usize) % spinners.len()];
        let just_refreshed = self.last_refresh.elapsed() < Duration::from_millis(800);

        // Main layout
        let top = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        // Header
        let phase = (self.tick % 120) as f32 / 120.0;
        let title_line = grad_sweep(" Containers ", theme.accent, theme.accent_alt, phase);

        let sort_name = match self.sort_key {
            SortKey::Name => "name",
            SortKey::Image => "image",
            SortKey::State => "state",
            SortKey::Created => "created",
            SortKey::Ports => "ports",
        };
        let arrow = if self.sort_asc { "↑" } else { "↓" };
        let sel_count = self.selected_ids.len();

        let mut hint = vec![Span::raw(" ")];
        hint.extend(title_line.spans);
        hint.push(Span::raw(
            "  a: all/running • o/O: sort • space/↵: start/stop • R: restart • p/P: pause • b: shell • x: select • A: apply • C: clear • i: inspect • l: logs • t: stats • S: save • d/D: delete "
        ));
        if !self.query.is_empty() {
            hint.push(Span::styled(
                format!(" | filter: '{}'", self.query),
                Style::default().fg(theme.accent),
            ));
        }
        hint.push(Span::styled(
            format!(" | sort: {sort_name}{arrow}"),
            Style::default().fg(theme.muted),
        ));
        if sel_count > 0 {
            hint.push(Span::styled(
                format!(" | selected: {sel_count}"),
                Style::default().fg(theme.accent),
            ));
        }
        if just_refreshed {
            hint.push(Span::styled(
                format!(" {spin}"),
                Style::default().fg(theme.accent),
            ));
        }
        let bar = Paragraph::new(Line::from(hint)).block(theme.block("List"));
        f.render_widget(bar, top[0]);

        // Table — filtered + sorted view
        let vis = self.visible_indices();
        let header = Row::new(vec!["", "NAME", "IMAGE", "STATE", "PORTS"]).style(
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        );

        let selected_row = self.state.selected().unwrap_or(0);
        let rows = vis.iter().enumerate().map(|(i, &idx)| {
            let c = &self.rows[idx];
            let icon = match c.state.as_ref() {
                Some(bollard::models::ContainerSummaryStateEnum::RUNNING) => Span::styled(
                    "●",
                    Style::default().fg(lerp(theme.ok, theme.accent, pulse)),
                ),
                Some(bollard::models::ContainerSummaryStateEnum::PAUSED) => {
                    Span::styled("■", Style::default().fg(theme.warn))
                }
                Some(bollard::models::ContainerSummaryStateEnum::EXITED) => {
                    Span::styled("●", Style::default().fg(theme.err))
                }
                _ => Span::raw("·"),
            };
            let id = c.id.as_deref().unwrap_or_default();
            let name_raw = c
                .names
                .as_ref()
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default();
            let name = if self.selected_ids.contains(id) {
                format!("▣ {}", name_raw)
            } else {
                format!("▢ {}", name_raw)
            };

            let image = c.image.clone().unwrap_or_default();
            let state_txt = c
                .state
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let state_upper = state_txt.to_uppercase();
            let (lbl, bg) = match state_txt.as_str() {
                "running" => ("RUNNING", theme.ok),
                "paused" => ("PAUSED", theme.warn),
                "exited" => ("EXITED", theme.err),
                _other => (state_upper.as_str(), theme.muted),
            };
            let badge = Span::styled(
                format!(" {lbl} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            );
            let ports = c
                .ports
                .as_ref()
                .map(|ps| {
                    ps.iter()
                        .map(|p| {
                            let privp = p.private_port;
                            let pubp = p.public_port.unwrap_or(0);
                            let typ = p
                                .typ
                                .as_ref()
                                .map(|t| format!("{:?}", t))
                                .unwrap_or_else(|| "tcp".to_string());
                            if pubp > 0 {
                                format!("{pubp}->{priv}/{typ}", priv=privp, typ=typ)
                            } else {
                                format!("{priv}/{typ}",     priv=privp, typ=typ)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("  ")
                })
                .unwrap_or_default();

            let mut row = Row::new(vec![
                Cell::from(Line::from(vec![icon])),
                Cell::from(name),
                Cell::from(truncate_middle(&image, 36)),
                Cell::from(Line::from(vec![badge])),
                Cell::from(ports),
            ]);

            if i == selected_row {
                row = row.style(selected_row_style(theme, self.tick));
            } else if i % 2 == 1 {
                row = row.style(alt_row_style());
            }
            row
        });

        let widths = [
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Percentage(26),
            Constraint::Length(12),
            Constraint::Percentage(30),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.muted))
                    .title(theme.title("Containers")),
            )
            .highlight_symbol("❯ ");
        f.render_stateful_widget(table, top[1], &mut self.state);

        // ---------- ACTION OVERLAY (start/stop) ----------
        if let Some(anim) = &self.anim {
            let w = (area.width / 3).clamp(38, 60);
            let h = area.height.saturating_sub(6).clamp(18, 999);
            let x = area.x + (area.width - w) / 2;
            let y = area.y + 3;
            let overlay = Rect {
                x,
                y,
                width: w,
                height: h,
            };

            f.render_widget(Clear, overlay);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(self.theme.title(match anim.kind {
                    ActionKind::Starting => "Launching…",
                    ActionKind::Stopping => "Landing…",
                }))
                .border_style(Style::default().fg(self.theme.accent));
            f.render_widget(block, overlay);

            let inner = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(h.saturating_sub(7)),
                    Constraint::Length(5),
                ])
                .split(Rect {
                    x: overlay.x + 2,
                    y: overlay.y + 1,
                    width: overlay.width.saturating_sub(4),
                    height: overlay.height.saturating_sub(2),
                });

            let title = Paragraph::new(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    &anim.name,
                    Style::default()
                        .fg(self.theme.fg)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            f.render_widget(title, inner[0]);

            let rocket = render_rocket_scene_vertical(
                anim.kind,
                inner[1].width,
                inner[1].height,
                anim.rocket_t,
                self.tick,
            );
            let rocket_para = Paragraph::new(rocket).wrap(Wrap { trim: false });
            f.render_widget(rocket_para, inner[1]);

            let phase_label = match anim.bar_phase {
                BarPhase::Init => "init",
                BarPhase::PullingImage => "pull image",
                BarPhase::StartingRuntime => "runtime",
                BarPhase::WaitingRunning => "running…",
                BarPhase::WaitingHealthy => "health…",
                BarPhase::StoppingSignal => "SIGTERM",
                BarPhase::WaitingExit => "exit…",
                BarPhase::Done => "done",
                BarPhase::Error => "error",
            };
            let bar_line = fancy_bar_line(
                inner[2].width as usize,
                anim.bar_pct,
                self.tick,
                self.theme.accent,
                self.theme.accent_alt,
            );
            let bar_para = Paragraph::new(bar_line).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Progress ({phase_label})")),
            );
            f.render_widget(bar_para, inner[2]);
        }

        // ---------- POPUP INSPECT ----------
        if let Some(Popup::Inspect(txt)) = &self.popup {
            let w = (area.width * 3 / 4).max(40);
            let h = (area.height * 3 / 4).max(12);
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
                .title(self.theme.title("Inspect (esc to close)"))
                .border_style(Style::default().fg(self.theme.accent));
            f.render_widget(block, overlay);

            let para = Paragraph::new(Text::raw(txt.clone())).wrap(Wrap { trim: false });
            f.render_widget(para, inner);
        }

        // ---------- POPUP CONFIRM DELETE ----------
        if let Some(Popup::ConfirmDelete { items }) = &self.popup {
            let w = (area.width / 2).max(48);
            let h = 8u16;
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
                .title(self.theme.title("Delete? (y/n/esc)"))
                .border_style(Style::default().fg(self.theme.err));
            f.render_widget(block, overlay);

            let msg = if items.len() == 1 {
                format!("Confirm deletion of {}?", items[0].1)
            } else {
                format!("Confirm deletion of {} containers?", items.len())
            };
            let para = Paragraph::new(Text::raw(msg)).wrap(Wrap { trim: false });
            f.render_widget(para, inner);
        }

        // ---------- SHELL POPUP ----------
        if let Some(shell) = &self.shell {
            shell.draw(f, area, self.tick);
        }
    }

    // =============== Helpers =================

    fn cycle_sort(&mut self) {
        self.sort_key = match self.sort_key {
            SortKey::Name => SortKey::Image,
            SortKey::Image => SortKey::State,
            SortKey::State => SortKey::Created,
            SortKey::Created => SortKey::Ports,
            SortKey::Ports => SortKey::Name,
        };
    }

    /// Indices of visible rows (filtered + sorted)
    fn visible_indices(&self) -> Vec<usize> {
        // 1) filtering
        let (tokens, label_filters) = parse_query(&self.query);
        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, c)| match_visible(c, &tokens, &label_filters))
            .map(|(i, _)| i)
            .collect();

        // 2) sorting
        indices.sort_by(|&a, &b| {
            let ca = &self.rows[a];
            let cb = &self.rows[b];
            let ord = match self.sort_key {
                SortKey::Name => key_name(ca).cmp(&key_name(cb)),
                SortKey::Image => key_image(ca).cmp(&key_image(cb)),
                SortKey::State => key_state(ca).cmp(&key_state(cb)),
                SortKey::Created => key_created(ca).cmp(&key_created(cb)),
                SortKey::Ports => key_ports(ca).cmp(&key_ports(cb)),
            };
            if self.sort_asc { ord } else { ord.reverse() }
        });

        indices
    }

    fn save_visible_to_tmp(&self) -> Result<std::path::PathBuf> {
        let mut path = std::env::temp_dir();
        path.push("dockrtui_containers.txt");
        let vis = self.visible_indices();
        let mut out = String::new();
        out.push_str("STATE\tNAME\tIMAGE\tPORTS\tID\n");
        for idx in vis {
            let c = &self.rows[idx];
            let id = c.id.as_deref().unwrap_or("");
            let name = c
                .names
                .as_ref()
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default();
            let image = c.image.clone().unwrap_or_default();
            let state = c
                .state
                .as_ref()
                .map(|s| format!("{:?}", s))
                .unwrap_or_else(|| "unknown".to_string());
            let ports = c
                .ports
                .as_ref()
                .map(|ps| {
                    ps.iter()
                        .map(|p| {
                            let privp = p.private_port;
                            let pubp = p.public_port.unwrap_or(0);
                            let typ = p
                                .typ
                                .as_ref()
                                .map(|t| format!("{:?}", t))
                                .unwrap_or_else(|| "tcp".to_string());
                            if pubp > 0 {
                                format!("{pubp}->{priv}/{typ}", priv=privp, typ=typ)
                            } else {
                                format!("{priv}/{typ}",     priv=privp, typ=typ)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            out.push_str(&format!("{state}\t{name}\t{image}\t{ports}\t{id}\n"));
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }
}

// ---------------- Filtering & sorting helpers ----------------

fn parse_query(q: &str) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut tokens = Vec::new();
    let mut labels = Vec::new();
    for tok in q.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('@') {
            if let Some((k, v)) = rest.split_once('=') {
                labels.push((k.to_lowercase(), Some(v.to_lowercase())));
            } else {
                labels.push((rest.to_lowercase(), None));
            }
        } else {
            tokens.push(tok.to_lowercase());
        }
    }
    (tokens, labels)
}

fn match_visible(
    c: &ContainerSummary,
    tokens: &[String],
    label_filters: &[(String, Option<String>)],
) -> bool {
    // free-text search
    let name = c
        .names
        .as_ref()
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    let image = c.image.clone().unwrap_or_default().to_lowercase();
    let state = c
        .state
        .as_ref()
        .map(|s| format!("{:?}", s))
        .unwrap_or_default()
        .to_lowercase();
    let ports = c
        .ports
        .as_ref()
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let privp = p.private_port;
                    let pubp = p.public_port.unwrap_or(0);
                    let typ = p
                        .typ
                        .as_ref()
                        .map(|t| format!("{:?}", t))
                        .unwrap_or_else(|| "tcp".to_string());
                    if pubp > 0 {
                        format!("{pubp}->{priv}/{typ}", priv=privp, typ=typ)
                    } else {
                        format!("{priv}/{typ}",     priv=privp, typ=typ)
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .to_lowercase()
        })
        .unwrap_or_default();

    let text_ok = tokens
        .iter()
        .all(|t| name.contains(t) || image.contains(t) || state.contains(t) || ports.contains(t));

    if !text_ok {
        return false;
    }

    // label filters (@key or @key=value)
    if label_filters.is_empty() {
        return true;
    }
    let labels: &HashMap<String, String> = match c.labels.as_ref() {
        Some(map) => map,
        None => return false,
    };
    label_filters.iter().all(|(k, v)| {
        let mut found = false;
        for (lk, lv) in labels {
            if lk.to_lowercase().contains(k) {
                if let Some(vv) = v {
                    if lv.to_lowercase().contains(vv) {
                        found = true;
                        break;
                    }
                } else {
                    found = true;
                    break;
                }
            }
        }
        found
    })
}

// sort keys
fn key_name(c: &ContainerSummary) -> String {
    c.names
        .as_ref()
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default()
        .to_lowercase()
}
fn key_image(c: &ContainerSummary) -> String {
    c.image.clone().unwrap_or_default().to_lowercase()
}
fn key_state(c: &ContainerSummary) -> String {
    c.state
        .as_ref()
        .map(|s| format!("{:?}", s))
        .unwrap_or_else(|| "~".to_string())
        .to_lowercase()
}
fn key_created(c: &ContainerSummary) -> i64 {
    c.created.unwrap_or_default()
}
fn key_ports(c: &ContainerSummary) -> String {
    c.ports
        .as_ref()
        .map(|ps| ps.len().to_string())
        .unwrap_or_default()
}

// ---------------- Inspect formatting ----------------

fn format_inspect(name: &str, ins: &bollard::models::ContainerInspectResponse) -> String {
    let mut s = String::new();
    let id = ins.id.as_deref().unwrap_or("");
    let image = ins.image.as_deref().unwrap_or("");
    let state = ins.state.as_ref();
    let running = state.and_then(|s| s.running).unwrap_or(false);
    let status = state
        .and_then(|s| s.status.as_ref())
        .map(|x| format!("{:?}", x))
        .unwrap_or_else(|| "unknown".to_string());
    let health = state
        .and_then(|s| s.health.as_ref())
        .and_then(|h| h.status.as_ref())
        .map(|x| format!("{:?}", x))
        .unwrap_or_else(|| "-".to_string());
    let started = state
        .and_then(|s| s.started_at.as_ref())
        .map(|x| x.as_str())
        .unwrap_or("-");
    let finished = state
        .and_then(|s| s.finished_at.as_ref())
        .map(|x| x.as_str())
        .unwrap_or("-");

    s.push_str(&format!("# {name}\n\n"));
    s.push_str(&format!("ID      : {id}\n"));
    s.push_str(&format!("Image   : {image}\n"));
    s.push_str(&format!(
        "State   : {status}  (running={running}, health={health})\n"
    ));
    s.push_str(&format!("Started : {started}\n"));
    if !running {
        s.push_str(&format!("Finished: {finished}\n"));
    }

    if let Some(cfg) = ins.config.as_ref()
        && let Some(env) = cfg.env.as_ref()
        && !env.is_empty()
    {
        s.push_str("\n## Env\n");
        for e in env.iter().take(40) {
            s.push_str(&format!("- {e}\n"));
        }
        if env.len() > 40 {
            s.push_str(&format!("… (+{} env)\n", env.len() - 40));
        }
    }

    if let Some(networks) = ins
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    {
        s.push_str("\n## Networks\n");
        for (name, n) in networks {
            let ip = n.ip_address.as_deref().unwrap_or("-");
            let gw = n.gateway.as_deref().unwrap_or("-");
            s.push_str(&format!("- {name}: ip={ip} gw={gw}\n"));
        }
    }

    if let Some(mounts) = ins.mounts.as_ref()
        && !mounts.is_empty()
    {
        s.push_str("\n## Mounts\n");
        for m in mounts {
            let src = m.source.as_deref().unwrap_or("-");
            let dst = m.destination.as_deref().unwrap_or("-");
            let typ = m
                .typ
                .as_ref()
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|| "-".to_string());
            s.push_str(&format!("- [{typ}] {src} → {dst}\n"));
        }
    }

    if let Some(ps) = ins.network_settings.as_ref().and_then(|n| n.ports.as_ref())
        && !ps.is_empty()
    {
        s.push_str("\n## Ports\n");
        for (k, vs) in ps {
            if let Some(vs) = vs {
                for v in vs {
                    let host = v.host_ip.as_deref().unwrap_or("0.0.0.0");
                    let hp = v.host_port.as_deref().unwrap_or("?");
                    s.push_str(&format!("- {host}:{hp} -> {k}\n"));
                }
            } else {
                s.push_str(&format!("- {k}\n"));
            }
        }
    }

    s
}

fn toggle(set: &mut HashSet<String>, id: &str) {
    if set.contains(id) {
        set.remove(id);
    } else {
        set.insert(id.to_string());
    }
}
