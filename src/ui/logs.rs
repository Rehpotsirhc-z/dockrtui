use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use futures_lite::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::{docker::DockerClient, theme::Theme};

const MAX_LINES: usize = 5_000;

pub struct LogsPane {
    docker: DockerClient,
    pub visible: bool,
    pub container_id: Option<String>,

    // circular buffer of log lines
    lines: VecDeque<String>,

    // UI state
    paused: bool,  // freeze autoscroll, but keep buffering
    follow: bool,  // if true and scroll==0, always stick to the bottom
    wrap: bool,    // soft visual wrapping
    scroll: usize, // number of lines from the bottom (0 = bottom)
    last_view_rows: usize,

    // follow task (docker)
    follow_tx: Option<UnboundedSender<Control>>,
    follow_task: Option<JoinHandle<()>>,
    in_rx: Option<UnboundedReceiver<String>>,

    // feedback / timestamp
    last_change: Instant,
    pub last_note: Option<(String, Color)>,

    // search
    query: String,
    searching: bool,
    last_match_row: Option<usize>,
}

enum Control {
    Stop,
}

impl LogsPane {
    pub fn new(docker: DockerClient) -> Self {
        Self {
            docker,
            visible: false,
            container_id: None,
            lines: VecDeque::with_capacity(MAX_LINES),
            paused: false,
            follow: true,
            wrap: false,
            scroll: 0,
            last_view_rows: 0,
            follow_tx: None,
            follow_task: None,
            in_rx: None,
            last_change: Instant::now(),
            last_note: None,
            query: String::new(),
            searching: false,
            last_match_row: None,
        }
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        if !self.visible {
            self.stop_follow();
        }
    }

    pub fn attach(&mut self, id: &str) {
        let needs_restart = self.container_id.as_deref() != Some(id) || self.follow_task.is_none();
        self.container_id = Some(id.to_string());
        if needs_restart && self.visible {
            self.restart_follow();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            // ---- actions
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                self.note(
                    if self.paused {
                        "⏸ autoscroll paused"
                    } else {
                        "▶ autoscroll resumed"
                    },
                    Color::Yellow,
                );
                if !self.paused && self.follow {
                    self.scroll = 0;
                }
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                self.note(
                    if self.follow {
                        "📌 follow on"
                    } else {
                        "📌 follow off"
                    },
                    Color::Blue,
                );
                if self.follow {
                    self.scroll = 0;
                }
            }
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                self.note(
                    if self.wrap {
                        "↪ wrap on"
                    } else {
                        "↪ wrap off"
                    },
                    Color::Blue,
                );
            }
            KeyCode::Char('c') => {
                self.lines.clear();
                self.scroll = 0;
                self.note("🧹 logs cleared", Color::Blue);
            }
            KeyCode::Char('s') => match self.save_to_tmp_all() {
                Ok(p) => self.note(format!("💾 saved: {}", p.display()), Color::Green),
                Err(e) => self.note(format!("❌ save logs: {e}"), Color::Red),
            },
            KeyCode::Char('S') => match self.save_to_tmp_filtered() {
                Ok(p) => self.note(format!("💾 saved filtered: {}", p.display()), Color::Green),
                Err(e) => self.note(format!("❌ save filtered: {e}"), Color::Red),
            },

            // ---- search mode
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }
            KeyCode::Esc => {
                if self.searching {
                    self.searching = false;
                } else {
                    self.query.clear();
                    self.last_match_row = None;
                }
            }
            KeyCode::Backspace if self.searching => {
                self.query.pop();
            }
            KeyCode::Enter if self.searching => {
                self.searching = false;
                // jump to latest match (near the bottom)
                self.jump_to_match(true);
            }
            KeyCode::Char('n') if !self.searching && !self.query.is_empty() => {
                self.jump_to_match(true);
            }
            KeyCode::Char('N') if !self.searching && !self.query.is_empty() => {
                self.jump_to_match(false);
            }
            KeyCode::Char(ch) if self.searching => {
                self.query.push(ch);
            }

            // ---- scrolling
            KeyCode::Up => {
                self.scroll_up(1);
            }
            KeyCode::Down => {
                self.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.scroll_up(self.page_step());
            }
            KeyCode::PageDown => {
                self.scroll_down(self.page_step());
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll_to_top();
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_to_bottom();
            }

            _ => {}
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, theme: Theme) {
        if !self.visible {
            return;
        }

        // right-hand overlay (half of the screen)
        let pane = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Percentage(50)])
            .split(area)[1];

        // clear + block
        f.render_widget(Clear, pane);
        let title = match (&self.container_id, self.paused, self.follow) {
            (Some(id), true, _) => format!("Logs ({id}) — paused"),
            (Some(id), _, true) => format!("Logs ({id}) — follow"),
            (Some(id), _, false) => format!("Logs ({id})"),
            (None, _, _) => "Logs".into(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(theme.title(&title))
            .border_style(Style::default().fg(theme.muted));
        f.render_widget(block, pane);

        // inner area
        let inner = Rect {
            x: pane.x + 1,
            y: pane.y + 1,
            width: pane.width.saturating_sub(2),
            height: pane.height.saturating_sub(2),
        };

        // remember visible height for paging / search jumps
        self.last_view_rows = inner.height as usize;

        // substring filter (case-insensitive)
        let (filtered, total) = self.filtered_view();

        // visible window (scroll from the bottom)
        let max_rows = inner.height as usize;
        let end = filtered.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(max_rows);
        let slice = &filtered[start..end];

        // colored rendering + highlight
        let mut text = Text::default();
        for line in slice {
            let line_spans = self.render_line(line, theme);
            text.extend(Line::from(line_spans));
        }

        // footer state + help
        let mut footer = String::new();
        footer.push_str(if self.follow && self.scroll == 0 {
            " [FOLLOW]"
        } else {
            " [SCROLL]"
        });
        if self.paused {
            footer.push_str(" [PAUSE]");
        }
        if self.wrap {
            footer.push_str(" [WRAP]");
        }
        footer.push_str(&format!(
            "  lines: {} (filtered: {})",
            total,
            filtered.len()
        ));
        footer.push_str(&format!("  pos: {}/{}", end, filtered.len()));
        if self.searching {
            footer.push_str(&format!("  | search: {}", self.query));
        } else if !self.query.is_empty() {
            footer.push_str(&format!("  | filter: '{}'", self.query));
        }
        footer.push_str(
            "  — keys: p pause • f follow • w wrap • ↑/↓/PgUp/PgDn/Home/End • s/S save • / search • n/N next/prev • l/esc close",
        );

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let mut para = Paragraph::new(text);
        if self.wrap {
            para = para.wrap(Wrap { trim: false });
        }
        f.render_widget(para, layout[0]);

        let foot = Paragraph::new(footer)
            .style(Style::default().fg(theme.muted).add_modifier(Modifier::DIM));
        f.render_widget(foot, layout[1]);
    }

    pub fn on_tick(&mut self) {
        // drain incoming from docker channel -> local buffer
        self.drain_incoming(1000);
        // autoscroll when follow is enabled
        if self.follow && !self.paused {
            self.scroll = 0;
        }
        let _ = &self.last_change;
    }

    pub fn stop_follow(&mut self) {
        if let Some(tx) = self.follow_tx.take() {
            let _ = tx.send(Control::Stop);
        }
        if let Some(t) = self.follow_task.take() {
            t.abort();
        }
        self.in_rx = None;
    }

    pub fn restart_follow(&mut self) {
        self.stop_follow();
        self.lines.clear();
        self.scroll = 0;
        self.follow = true;

        if let Some(id) = self.container_id.clone() {
            let (ctx, mut crx) = mpsc::unbounded_channel::<Control>();
            let (ltx, lrx) = mpsc::unbounded_channel::<String>();
            let docker = self.docker.clone();

            // task reading docker logs in follow mode and sending lines
            let handle = tokio::spawn(async move {
                let mut stream = match docker.logs_stream(&id, true).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                loop {
                    tokio::select! {
                        _ = crx.recv() => break,
                        maybe = stream.next() => {
                            match maybe {
                                Some(Ok(line)) => { let _ = ltx.send(line); }
                                Some(Err(_)) => { /* ignore */ }
                                None => break,
                            }
                        }
                    }
                }
            });

            self.follow_tx = Some(ctx);
            self.follow_task = Some(handle);
            self.in_rx = Some(lrx);
        }
    }

    pub fn drain_incoming(&mut self, max_lines: usize) {
        let mut collected = Vec::new();
        let mut channel_closed = false;

        if let Some(rx) = self.in_rx.as_mut() {
            let mut n = 0usize;
            while n < max_lines {
                match rx.try_recv() {
                    Ok(line) => {
                        collected.push(line);
                        n += 1;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(_) => {
                        channel_closed = true;
                        break;
                    }
                }
            }
        }

        if channel_closed {
            self.in_rx = None;
        }

        for line in collected {
            self.push_line(line);
        }
    }

    fn save_to_tmp_all(&self) -> Result<PathBuf> {
        self.save_lines(&self.lines.iter().cloned().collect::<Vec<_>>())
    }

    fn save_to_tmp_filtered(&self) -> Result<PathBuf> {
        let (filtered, _total) = self.filtered_view();
        self.save_lines(&filtered.to_vec())
    }

    fn save_lines(&self, lines: &[String]) -> Result<PathBuf> {
        let mut path = std::env::temp_dir();
        let name = self
            .container_id
            .as_deref()
            .unwrap_or("unknown")
            .trim_start_matches('/');
        path.push(format!("dockrtui_logs_{}.txt", name));
        std::fs::write(&path, lines.join("\n"))?;
        Ok(path)
    }

    fn note(&mut self, msg: impl Into<String>, color: Color) {
        self.last_note = Some((msg.into(), color));
        self.last_change = Instant::now();
    }

    fn push_line(&mut self, mut s: String) {
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
        }
        if s.ends_with('\r') {
            s.pop();
        } // strip CR
        self.lines.push_back(s);
    }

    /* ---------------- helpers: render & search ---------------- */

    fn filtered_view(&self) -> (Vec<String>, usize) {
        let total = self.lines.len();
        if self.query.is_empty() {
            (self.lines.iter().cloned().collect(), total)
        } else {
            let q = self.query.to_lowercase();
            let v = self
                .lines
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .cloned()
                .collect::<Vec<_>>();
            (v, total)
        }
    }

    fn render_line(&self, line: &str, theme: Theme) -> Vec<Span<'static>> {
        let mut spans = Vec::new();

        // optional timestamp prefix at the beginning of the line
        if let Some(ts_end) = guess_timestamp_end(line) {
            let (ts, rest) = line.split_at(ts_end);
            spans.push(Span::styled(
                ts.to_string(),
                Style::default().fg(theme.muted),
            ));
            spans.push(Span::raw(rest.to_string()));
            return self.highlight_level_and_query(spans, theme);
        }

        // no dedicated timestamp
        spans.push(Span::raw(line.to_string()));
        self.highlight_level_and_query(spans, theme)
    }

    fn highlight_level_and_query(
        &self,
        chunks: Vec<Span<'static>>,
        theme: Theme,
    ) -> Vec<Span<'static>> {
        // 1) colorize level (error/warn/debug)
        let mut recolored: Vec<Span> = Vec::new();
        for sp in chunks {
            let s = sp.content.to_string();
            let style = level_style(&s, theme);
            recolored.push(Span::styled(s, style));
        }

        // 2) highlight query (case-insensitive) while keeping base style
        if self.query.is_empty() {
            return recolored;
        }
        let q = self.query.to_lowercase();
        let hi_style = Style::default()
            .bg(theme.accent)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);

        let mut final_spans: Vec<Span> = Vec::new();
        for sp in recolored {
            let s = sp.content.to_string();
            let base = sp.style;
            let mut i = 0usize;
            let lower = s.to_lowercase();
            while i < s.len() {
                if let Some(pos) = lower[i..].find(&q) {
                    let from = i;
                    let to = i + pos;
                    if from < to {
                        final_spans.push(Span::styled(s[from..to].to_string(), base));
                    }
                    let to2 = to + q.len();
                    final_spans.push(Span::styled(s[to..to2].to_string(), base.patch(hi_style)));
                    i = to2;
                } else {
                    final_spans.push(Span::styled(s[i..].to_string(), base));
                    break;
                }
            }
        }
        final_spans
    }

    fn jump_to_match(&mut self, forward: bool) {
        if self.query.is_empty() {
            return;
        }
        let (filtered, _total) = self.filtered_view();
        if filtered.is_empty() {
            return;
        }

        let start_idx = self.last_match_row.unwrap_or_else(|| {
            if forward {
                0
            } else {
                filtered.len().saturating_sub(1)
            }
        });

        let q = self.query.to_lowercase();
        let next = if forward {
            (start_idx..filtered.len())
                .find(|&i| filtered[i].to_lowercase().contains(&q))
                .or_else(|| (0..start_idx).find(|&i| filtered[i].to_lowercase().contains(&q)))
        } else {
            (0..=start_idx)
                .rev()
                .find(|&i| filtered[i].to_lowercase().contains(&q))
                .or_else(|| {
                    (start_idx + 1..filtered.len())
                        .rev()
                        .find(|&i| filtered[i].to_lowercase().contains(&q))
                })
        };

        if let Some(idx) = next {
            self.last_match_row = Some(idx);
            // place the matched line at the bottom of the viewport
            let end = idx + 1;
            let view = self.last_view_rows.max(1);
            let end_clamped = end.min(filtered.len());
            let _start = end_clamped.saturating_sub(view);
            // scroll = number of lines from the bottom
            self.scroll = filtered.len().saturating_sub(end_clamped);
            self.follow = false;
            self.paused = true; // freeze autoscroll so the user can read
        }
    }

    /* ---------------- helpers: scroll ---------------- */

    fn page_step(&self) -> usize {
        self.last_view_rows.saturating_sub(1).max(1)
    }

    fn scroll_up(&mut self, n: usize) {
        self.scroll = self
            .scroll
            .saturating_add(n)
            .min(self.total_filtered_len().saturating_sub(1));
        self.follow = false;
        self.paused = true;
    }

    fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        if self.scroll == 0 && self.follow {
            self.paused = false;
        }
    }

    fn scroll_to_top(&mut self) {
        self.scroll = self.total_filtered_len().saturating_sub(1);
        self.follow = false;
        self.paused = true;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        if self.follow {
            self.paused = false;
        }
    }

    fn total_filtered_len(&self) -> usize {
        if self.query.is_empty() {
            self.lines.len()
        } else {
            let q = self.query.to_lowercase();
            self.lines
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .count()
        }
    }
}

/* ---------------- utilities ---------------- */

fn level_style(s: &str, theme: Theme) -> Style {
    let l = s.to_lowercase();
    if l.contains("error") || l.contains(" level=error") || l.contains(" err ") || l.contains(" e ")
    {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if l.contains("warn") || l.contains(" level=warn") {
        Style::default().fg(Color::Yellow)
    } else if l.contains("debug") || l.contains("trace") {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(theme.fg)
    }
}

// Try to detect an ISO-like timestamp or "YYYY-MM-DD HH:MM:SS" prefix at the start of the line.
fn guess_timestamp_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    // minimal length for "YYYY-MM-DDTHH:MM:SS" = 19
    if bytes.len() < 19 {
        return None;
    }
    // formats: "YYYY-MM-DDTHH:MM:SS" or "YYYY-MM-DD HH:MM:SS"
    let check = |i: usize, ch: u8| -> bool { bytes.get(i).copied() == Some(ch) };
    let is_digit = |i: usize| -> bool { matches!(bytes.get(i), Some(b'0'..=b'9')) };
    let looks_iso = is_digit(0)
        && is_digit(1)
        && is_digit(2)
        && is_digit(3)
        && check(4, b'-')
        && is_digit(5)
        && is_digit(6)
        && check(7, b'-')
        && is_digit(8)
        && is_digit(9)
        && (check(10, b'T') || check(10, b' '))
        && is_digit(11)
        && is_digit(12)
        && check(13, b':')
        && is_digit(14)
        && is_digit(15)
        && check(16, b':')
        && is_digit(17)
        && is_digit(18);

    if looks_iso {
        // extend until the end of the timestamp (millis + tz if present)
        let mut i = 19;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_whitespace() {
                break;
            }
            // accept .123, Z, +02:00, etc.
            if !(c.is_ascii_digit()
                || c == b'.'
                || c == b'Z'
                || c == b'+'
                || c == b'-'
                || c == b':')
            {
                break;
            }
            i += 1;
        }
        return Some(i);
    }
    None
}
