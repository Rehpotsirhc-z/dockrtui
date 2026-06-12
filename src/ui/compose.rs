use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use serde_json::Value;

use crate::ui::containers;
use crate::ui::pull::{PullPopup, spawn_compose_run};
use crate::{docker::DockerClient, theme::Theme};
use containers::util::{grad_sweep, truncate_middle};

#[derive(Clone)]
struct ComposeProject {
    name: String,
    path: PathBuf,     // directory
    file_name: String, // compose file
    status: Option<String>,
}

pub struct ComposeView {
    _docker: DockerClient, // kept for potential future use
    theme: Theme,
    tick: u64,

    rows: Vec<ComposeProject>,
    state: TableState,

    last_scan: Instant,
    searching: bool,
    query: String,

    pull: PullPopup,
    pub last_note: Option<(String, Color)>,
}

impl ComposeView {
    pub async fn new(docker: DockerClient, theme: Theme) -> Result<Self> {
        let mut s = Self {
            _docker: docker,
            theme,
            tick: 0,
            rows: Vec::new(),
            state: TableState::default(),
            last_scan: Instant::now(),
            searching: false,
            query: String::new(),
            pull: PullPopup::new(),
            last_note: None,
        };
        s.scan_projects()?;
        Ok(s)
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.pull.on_tick();
        if let Some(ok) = self.pull.take_finished() {
            let _ = self.scan_projects();
            if ok {
                self.note_ok("✅ compose command complete");
            } else {
                self.note_err("❌ compose command finished with errors");
            }
        }
    }

    pub fn is_modal_open(&self) -> bool {
        self.pull.visible || self.searching
    }

    /// For compatibility with Ui which calls `has_modal()`
    pub fn has_modal(&self) -> bool {
        self.is_modal_open()
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

    /// Scan compose files in the current directory,
    /// then enrich with `docker compose ls --format json`
    fn scan_projects(&mut self) -> Result<()> {
        // 1) projects known by `docker compose ls`
        let mut rows = self.projects_from_docker_ls();

        // 2) compose files in the current directory
        let mut from_fs = self.projects_from_fs()?;
        rows.append(&mut from_fs);

        // 3) deduplicate by (path, file_name)
        use std::collections::HashMap;
        let mut map: HashMap<(PathBuf, String), ComposeProject> = HashMap::new();
        for p in rows {
            let key = (p.path.clone(), p.file_name.clone());
            map.insert(key, p);
        }

        let mut rows: Vec<ComposeProject> = map.into_values().collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.file_name.cmp(&b.file_name)));

        self.rows = rows;
        self.last_scan = Instant::now();

        // clamp selection
        let len = self.visible_indices().len();
        match self.state.selected() {
            Some(sel) if sel >= len => {
                self.state
                    .select(if len == 0 { None } else { Some(len - 1) });
            }
            None if len > 0 => self.state.select(Some(0)),
            _ => {}
        }

        Ok(())
    }

    fn projects_from_docker_ls(&self) -> Vec<ComposeProject> {
        let mut out = Vec::new();

        let output = Command::new("docker")
            .args(["compose", "ls", "--format", "json"])
            .output();

        let Ok(output) = output else { return out };
        if !output.status.success() {
            return out;
        }

        let json = String::from_utf8_lossy(&output.stdout);
        let val: Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => return out,
        };
        let arr = match val.as_array() {
            Some(a) => a,
            None => return out,
        };

        for entry in arr {
            let name = entry
                .get("Name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = entry
                .get("Status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Try to retrieve compose file(s) from ConfigFiles
            let cfg_raw = entry
                .get("ConfigFiles")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let path_from_cfg = cfg_raw
                .split(',')
                .map(|s| s.trim())
                .find(|s| !s.is_empty())
                .map(PathBuf::from);

            let (path, file_name) = if let Some(p) = path_from_cfg {
                let file_name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("docker-compose.yml")
                    .to_string();
                let dir = p.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
                (dir, file_name)
            } else {
                // fallback: WorkingDir + docker-compose.yml
                let wd = entry
                    .get("WorkingDir")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let dir = if wd.is_empty() {
                    PathBuf::from(".")
                } else {
                    PathBuf::from(wd)
                };
                (dir, "docker-compose.yml".to_string())
            };

            out.push(ComposeProject {
                name: if name.is_empty() {
                    file_name.clone()
                } else {
                    name
                },
                path,
                file_name,
                status: if status.is_empty() {
                    None
                } else {
                    Some(status)
                },
            });
        }

        out
    }

    /// Projects detected by scanning the current directory
    fn projects_from_fs(&self) -> Result<Vec<ComposeProject>> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let candidates = [
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yml",
            "compose.yaml",
        ];

        let mut rows = Vec::new();

        for entry in fs::read_dir(&cwd)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let fname = if let Some(n) = path.file_name().and_then(|s| s.to_str()) {
                n.to_string()
            } else {
                continue;
            };
            if !candidates.contains(&fname.as_str()) {
                continue;
            }

            let dir = path.parent().unwrap_or(&cwd).to_path_buf();
            let name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(".")
                .to_string();

            rows.push(ComposeProject {
                name,
                path: dir,
                file_name: fname,
                status: None,
            });
        }

        Ok(rows)
    }

    fn visible_indices(&self) -> Vec<usize> {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();

        let mut idx: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, p)| match_visible(p, &tokens))
            .map(|(i, _)| i)
            .collect();

        idx.sort_by(|&a, &b| {
            let pa = &self.rows[a];
            let pb = &self.rows[b];
            pa.name.cmp(&pb.name).then(pa.file_name.cmp(&pb.file_name))
        });

        idx
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

    /// Return a *copy* to avoid borrow conflicts with &mut self
    fn current_project(&self) -> Option<ComposeProject> {
        let idx = self.state.selected()?;
        let vis = self.visible_indices();
        let row_idx = *vis.get(idx)?;
        self.rows.get(row_idx).cloned()
    }

    fn compose_targets(&self) -> Vec<(PathBuf, String, String)> {
        self.current_project()
            .map(|p| vec![(p.path, p.file_name, p.name)])
            .unwrap_or_default()
    }

    fn run_compose(&mut self, verb: &str, subcommand: &[&str]) {
        let targets = self.compose_targets();
        if targets.is_empty() {
            self.note_warn("⚠ no project selected");
            return;
        }
        let title = if targets.len() == 1 {
            format!("compose {verb} ({})", targets[0].2)
        } else {
            format!("compose {verb} ({} projects)", targets.len())
        };
        let sub = subcommand.iter().map(|s| s.to_string()).collect();
        let (rx, handle) = spawn_compose_run(targets, sub);
        self.pull.start(title, rx, handle);
        self.note_ok(format!("⏬ compose {verb}…"));
    }

    /// Launch $EDITOR (or nano) on the selected compose file
    fn edit_current_file(&mut self) {
        if let Some(p) = self.current_project() {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
            let full = p.path.join(&p.file_name);
            match Command::new(&editor).arg(&full).status() {
                Ok(_) => self.note_ok(format!("✏️ edited: {}", full.display())),
                Err(e) => self.note_err(format!("❌ failed to launch editor: {e}")),
            }
        } else {
            self.note_warn("⚠ no project selected");
        }
    }

    pub async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // pull popup owns the keyboard while visible
        if self.pull.visible {
            self.pull.on_key(key);
            return Ok(());
        }

        // search mode
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
            let len = vis.len();
            let cur = self.state.selected().unwrap_or(0);
            if cur >= len {
                self.state
                    .select(if len == 0 { None } else { Some(len - 1) });
            }
            return Ok(());
        }

        // normal shortcuts
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),

            KeyCode::Char('r') | KeyCode::F(5) => {
                self.scan_projects()?;
                self.note_ok("✅ compose projects rescanned");
            }

            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }

            // edit
            KeyCode::Char('e') => {
                self.edit_current_file();
            }

            // docker compose up -d
            KeyCode::Char('u') => self.run_compose("up -d", &["up", "-d"]),

            // docker compose down
            KeyCode::Char('d') => self.run_compose("down", &["down"]),

            // docker compose pull
            KeyCode::Char('p') => self.run_compose("pull", &["pull"]),

            // docker compose ps
            KeyCode::Char('s') => self.run_compose("ps", &["ps"]),

            // docker compose logs --tail=50
            KeyCode::Char('l') => self.run_compose("logs", &["logs", "--tail=50"]),

            _ => {}
        }

        Ok(())
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.theme;

        let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = spinners[(self.tick as usize) % spinners.len()];
        let just_scanned = self.last_scan.elapsed() < Duration::from_millis(800);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        // header / hints
        let phase = (self.tick % 120) as f32 / 120.0;
        let title_line = grad_sweep(" Compose ", theme.accent, theme.accent_alt, phase);

        let mut spans = vec![Span::raw(" ")];
        spans.extend(title_line.spans.clone());
        spans.push(Span::raw(
            "  j/k ↑/↓ • /: search • r/F5: rescan • u: up -d • d: down • p: pull • s: ps • l: logs • e: edit",
        ));

        if !self.query.is_empty() {
            spans.push(Span::styled(
                format!(" | filter: '{}'", self.query),
                Style::default().fg(theme.accent),
            ));
        }
        if just_scanned {
            spans.push(Span::styled(
                format!(" {spin}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(spans)).block(theme.block("Compose"));
        f.render_widget(header, layout[0]);

        // table
        let vis = self.visible_indices();
        let selected_row = self.state.selected().unwrap_or(0);

        let header_row = Row::new(vec!["PROJECT", "STATUS", "FILE", "PATH"]).style(
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        );

        let rows = vis.iter().enumerate().map(|(i, &idx)| {
            let p = &self.rows[idx];

            let proj = p.name.clone();
            let status = p.status.clone().unwrap_or_else(|| "-".into());
            let file = p.file_name.clone();
            let path_str = truncate_middle(p.path.to_string_lossy().as_ref(), 60);

            let mut row = Row::new(vec![
                Cell::from(proj),
                Cell::from(status),
                Cell::from(file),
                Cell::from(path_str),
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
            Constraint::Length(24),
            Constraint::Length(12),
            Constraint::Length(20),
            Constraint::Percentage(60),
        ];

        let table = Table::new(rows, widths)
            .header(header_row)
            .column_spacing(2)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.muted))
                    .title(theme.title("Compose")),
            )
            .highlight_symbol("❯ ");

        f.render_stateful_widget(table, layout[1], &mut self.state);

        // command output / progress
        self.pull.draw(f, area, self.theme, self.tick);
    }
}

/* =============== helpers =============== */

fn match_visible(p: &ComposeProject, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {}",
        p.name.to_lowercase(),
        p.file_name.to_lowercase(),
        p.status.as_deref().unwrap_or("-").to_lowercase(),
        p.path.to_string_lossy().to_string().to_lowercase()
    );
    tokens.iter().all(|t| hay.contains(t))
}
