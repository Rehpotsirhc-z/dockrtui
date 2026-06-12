//! Async image-pull popup, shared by the Images and Compose tabs.
//!
//! Two very different progress sources feed the same popup:
//!
//! * **Images** pull through `bollard` and arrive as *structured* events
//!   (`CreateImageInfo`: a layer id + status + progress string). Those are
//!   rendered as keyed lines (repeated updates for a layer overwrite the same
//!   line instead of scrolling).
//!
//! * **Compose** shells out to `docker compose pull`, which writes a live
//!   *terminal* stream: it moves the cursor up and rewrites its block in place
//!   (the `[+] pull N/N` view with one line per image). To mirror that exactly
//!   we run a tiny terminal emulator ([`Screen`]) over the raw bytes, honouring
//!   carriage returns, cursor moves and line erases.
//!
//! Either way the work runs on a background tokio task and streams `PullEvent`s
//! back over a channel, so the UI never blocks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use bollard::models::CreateImageInfo;
use crossterm::event::{KeyCode, KeyEvent};
use futures_lite::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::{docker::DockerClient, theme::Theme};

/// A message produced by a running pull task.
pub enum PullEvent {
    /// A structured line (Images). When `key` is `Some`, it replaces any earlier
    /// line with the same key (collapsing repeated layer-progress updates).
    Line { key: Option<String>, text: String },
    /// A raw chunk of terminal output (Compose), fed to the screen emulator.
    Bytes(Vec<u8>),
    /// Terminal event: the whole pull finished.
    Done { ok: bool, summary: String },
}

pub struct PullPopup {
    pub visible: bool,
    title: String,

    // Rendered output, as a grid of cells. Driven either by `apply_keyed`
    // (Images) or by the terminal emulator (`screen.feed`, Compose).
    screen: Screen,
    // key -> row, for in-place updates of structured image-pull lines.
    index: HashMap<String, usize>,

    rx: Option<UnboundedReceiver<PullEvent>>,
    task: Option<JoinHandle<()>>,

    // `None` while running, `Some(ok)` once a Done event arrives.
    finished: Option<bool>,
    finished_taken: bool,
    summary: String,

    // scroll offset measured in lines from the bottom (0 == follow tail).
    scroll: usize,
    view_rows: usize,
}

impl PullPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            title: String::new(),
            screen: Screen::new(),
            index: HashMap::new(),
            rx: None,
            task: None,
            finished: None,
            finished_taken: false,
            summary: String::new(),
            scroll: 0,
            view_rows: 0,
        }
    }

    /// Open the popup and start consuming a freshly spawned pull task.
    pub fn start(
        &mut self,
        title: impl Into<String>,
        rx: UnboundedReceiver<PullEvent>,
        task: JoinHandle<()>,
    ) {
        self.stop();
        self.title = title.into();
        self.screen = Screen::new();
        self.index.clear();
        self.finished = None;
        self.finished_taken = false;
        self.summary.clear();
        self.scroll = 0;
        self.rx = Some(rx);
        self.task = Some(task);
        self.visible = true;
    }

    fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.rx = None;
    }

    /// Returns `Some(ok)` exactly once after the pull finishes, so the owning
    /// view can refresh its list and emit a toast.
    pub fn take_finished(&mut self) -> Option<bool> {
        match self.finished {
            Some(ok) if !self.finished_taken => {
                self.finished_taken = true;
                Some(ok)
            }
            _ => None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.stop();
                self.visible = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll());
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll = self
                    .scroll
                    .saturating_add(self.page_step())
                    .min(self.max_scroll());
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.page_step());
            }
            KeyCode::Home | KeyCode::Char('g') => self.scroll = self.max_scroll(),
            KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
            _ => {}
        }
    }

    /// Drain everything the pull task has produced since the last tick.
    pub fn on_tick(&mut self) {
        let mut events = Vec::new();
        let mut closed = false;
        if let Some(rx) = self.rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        if closed {
            self.rx = None;
        }
        for ev in events {
            match ev {
                PullEvent::Line { key, text } => self.apply_keyed(key, text),
                PullEvent::Bytes(bytes) => self.screen.feed(&bytes),
                PullEvent::Done { ok, summary } => {
                    self.finished = Some(ok);
                    self.summary = summary;
                }
            }
        }
    }

    /// Structured (Images) update: append, or overwrite the line for `key`.
    fn apply_keyed(&mut self, key: Option<String>, text: String) {
        let chars: Vec<char> = text.chars().collect();
        match key {
            Some(k) => match self.index.get(&k) {
                Some(&row) if row < self.screen.lines.len() => self.screen.lines[row] = chars,
                _ => {
                    self.index.insert(k, self.screen.lines.len());
                    self.screen.lines.push(chars);
                }
            },
            None => self.screen.lines.push(chars),
        }
    }

    fn page_step(&self) -> usize {
        self.view_rows.saturating_sub(1).max(1)
    }

    fn max_scroll(&self) -> usize {
        self.screen.lines.len().saturating_sub(1)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, theme: Theme, tick: u64) {
        if !self.visible {
            return;
        }

        let w = (area.width * 4 / 5).max(60).min(area.width);
        let h = (area.height * 4 / 5).max(12).min(area.height);
        let overlay = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, overlay);

        let head = match self.finished {
            None => {
                let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let spin = spinners[(tick as usize) % spinners.len()];
                format!("{} {} {}", spin, self.title, spin)
            }
            Some(true) => format!("✅ {}", self.title),
            Some(false) => format!("❌ {}", self.title),
        };
        let border = match self.finished {
            None => theme.accent,
            Some(true) => theme.ok,
            Some(false) => theme.err,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(theme.title(&head))
            .border_style(Style::default().fg(border));
        f.render_widget(block, overlay);

        let inner = Rect {
            x: overlay.x + 1,
            y: overlay.y + 1,
            width: overlay.width.saturating_sub(2),
            height: overlay.height.saturating_sub(2),
        };

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        // Render the cell grid into trimmed strings, then tail/scroll it.
        let rendered: Vec<String> = self
            .screen
            .lines
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect();

        let max_rows = layout[0].height as usize;
        self.view_rows = max_rows;
        let end = rendered.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(max_rows);
        let body: Vec<Line> = rendered[start..end]
            .iter()
            .map(|t| Line::from(Span::styled(t.clone(), line_style(t, theme))))
            .collect();
        f.render_widget(Paragraph::new(Text::from(body)), layout[0]);

        let footer = match &self.finished {
            None => "  pulling…  esc: cancel • ↑/↓/PgUp/PgDn: scroll".to_string(),
            Some(_) => format!("  {} — esc: close • ↑/↓: scroll", self.summary),
        };
        f.render_widget(
            Paragraph::new(footer)
                .style(Style::default().fg(theme.muted).add_modifier(Modifier::DIM)),
            layout[1],
        );
    }
}

fn line_style(text: &str, theme: Theme) -> Style {
    if text.starts_with('❌') {
        Style::default().fg(theme.err)
    } else if text.contains("complete")
        || text.contains("Pulled")
        || text.contains("Already exists")
        || text.contains("up to date")
        || text.contains("Downloaded")
    {
        Style::default().fg(theme.ok)
    } else {
        Style::default().fg(theme.fg)
    }
}

/* ============================ terminal emulator ============================ */

/// A minimal terminal screen: just enough of the ANSI/VT100 control set to
/// faithfully replay `docker compose`'s in-place progress output (carriage
/// returns, cursor up/down, column moves and line/screen erases). Colours
/// (SGR) and other sequences are parsed and ignored.
struct Screen {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// Bytes left over from an escape sequence or UTF-8 char split across reads.
    pending: Vec<u8>,
}

impl Screen {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            row: 0,
            col: 0,
            pending: Vec::new(),
        }
    }

    fn feed(&mut self, data: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);

        let n = buf.len();
        let mut i = 0;
        while i < n {
            let b = buf[i];
            match b {
                0x1b => match self.parse_escape(&buf, i) {
                    Some(next) => i = next,
                    None => break, // incomplete sequence: keep the rest for later
                },
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                b'\n' => {
                    self.newline();
                    i += 1;
                }
                b'\t' => {
                    self.col = (self.col / 8 + 1) * 8;
                    i += 1;
                }
                0x08 => {
                    self.col = self.col.saturating_sub(1);
                    i += 1;
                }
                _ if b < 0x20 => i += 1, // ignore other C0 controls
                _ => {
                    let len = utf8_len(b);
                    if i + len > n {
                        break; // incomplete UTF-8 char
                    }
                    let ch = std::str::from_utf8(&buf[i..i + len])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or('\u{fffd}');
                    self.put(ch);
                    i += len;
                }
            }
        }

        self.pending = buf[i..].to_vec();
    }

    /// Parse one escape sequence starting at `buf[i]` (== ESC). Returns the
    /// index just past it, or `None` if the buffer ends mid-sequence.
    fn parse_escape(&mut self, buf: &[u8], i: usize) -> Option<usize> {
        match *buf.get(i + 1)? {
            b'[' => {
                // CSI: parameter/intermediate bytes until a final byte (0x40..=0x7e).
                let mut j = i + 2;
                while j < buf.len() {
                    let c = buf[j];
                    if (0x40..=0x7e).contains(&c) {
                        self.dispatch_csi(&buf[i + 2..j], c);
                        return Some(j + 1);
                    }
                    j += 1;
                }
                None
            }
            b']' => {
                // OSC: skip until BEL or ST (ESC \).
                let mut j = i + 2;
                while j < buf.len() {
                    if buf[j] == 0x07 {
                        return Some(j + 1);
                    }
                    if buf[j] == 0x1b && buf.get(j + 1) == Some(&b'\\') {
                        return Some(j + 2);
                    }
                    j += 1;
                }
                None
            }
            // Charset-selection escapes (ESC ( X, etc.): 3 bytes total.
            b'(' | b')' | b'*' | b'+' => {
                if buf.len() > i + 2 {
                    Some(i + 3)
                } else {
                    None
                }
            }
            // Any other 2-byte escape: ignore.
            _ => Some(i + 2),
        }
    }

    fn dispatch_csi(&mut self, params: &[u8], final_byte: u8) {
        let text = std::str::from_utf8(params).unwrap_or("");
        let nums: Vec<usize> = text
            .split(';')
            .map(|p| p.trim_start_matches('?').parse::<usize>().unwrap_or(0))
            .collect();
        let p0 = nums.first().copied().unwrap_or(0);
        let n = p0.max(1);

        match final_byte {
            b'A' => self.row = self.row.saturating_sub(n),
            b'B' => self.row = self.row.saturating_add(n),
            b'C' => self.col = self.col.saturating_add(n),
            b'D' => self.col = self.col.saturating_sub(n),
            b'E' => {
                self.row = self.row.saturating_add(n);
                self.col = 0;
            }
            b'F' => {
                self.row = self.row.saturating_sub(n);
                self.col = 0;
            }
            b'G' => self.col = p0.saturating_sub(1),
            b'H' | b'f' => {
                self.row = nums.first().copied().unwrap_or(1).max(1) - 1;
                self.col = nums.get(1).copied().unwrap_or(1).max(1) - 1;
            }
            b'J' => self.erase_display(p0),
            b'K' => self.erase_line(p0),
            _ => {} // SGR (m), cursor show/hide (h/l), etc. — ignored
        }
    }

    fn erase_line(&mut self, mode: usize) {
        self.ensure_row(self.row);
        let line = &mut self.lines[self.row];
        match mode {
            0 => line.truncate(self.col), // cursor to end of line
            1 => {
                for c in line.iter_mut().take(self.col) {
                    *c = ' ';
                }
            }
            2 => line.clear(),
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.ensure_row(self.row);
                self.lines[self.row].truncate(self.col);
                self.lines.truncate(self.row + 1);
            }
            1 => {
                for r in 0..self.row {
                    if let Some(line) = self.lines.get_mut(r) {
                        line.clear();
                    }
                }
                self.ensure_row(self.row);
                for c in self.lines[self.row].iter_mut().take(self.col) {
                    *c = ' ';
                }
            }
            _ => {
                self.lines.clear();
                self.row = 0;
                self.col = 0;
            }
        }
    }

    fn put(&mut self, ch: char) {
        self.ensure_row(self.row);
        let line = &mut self.lines[self.row];
        while line.len() < self.col {
            line.push(' ');
        }
        if self.col < line.len() {
            line[self.col] = ch;
        } else {
            line.push(ch);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        self.ensure_row(self.row);
    }

    fn ensure_row(&mut self, row: usize) {
        while self.lines.len() <= row {
            self.lines.push(Vec::new());
        }
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/* ============================== producers ============================== */

/// Spawn a background task that pulls `images` one after another and reports
/// progress. Layer-progress lines are keyed by their layer id so they collapse
/// onto a single line per layer.
pub fn spawn_image_pull(
    docker: DockerClient,
    images: Vec<String>,
) -> (UnboundedReceiver<PullEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let total = images.len();
        let mut had_err = false;

        for (i, image) in images.iter().enumerate() {
            let _ = tx.send(PullEvent::Line {
                key: None,
                text: format!("⏬ Pulling {image} ({}/{total})", i + 1),
            });

            let mut stream = docker.pull_stream(image);
            while let Some(item) = stream.next().await {
                match item {
                    Ok(info) => {
                        if let Some((key, text)) = format_pull_info(image, &info) {
                            let _ = tx.send(PullEvent::Line { key, text });
                        }
                    }
                    Err(e) => {
                        had_err = true;
                        let _ = tx.send(PullEvent::Line {
                            key: None,
                            text: format!("❌ {image}: {e}"),
                        });
                        break;
                    }
                }
            }
        }

        let summary = if had_err {
            "finished with errors".to_string()
        } else if total == 1 {
            "pull complete".to_string()
        } else {
            format!("pulled {total} images")
        };
        let _ = tx.send(PullEvent::Done {
            ok: !had_err,
            summary,
        });
    });
    (rx, handle)
}

/// Turn a single `CreateImageInfo` event into a keyed output line. Returns
/// `None` for empty/uninteresting events.
fn format_pull_info(image: &str, info: &CreateImageInfo) -> Option<(Option<String>, String)> {
    if let Some(err) = &info.error {
        return Some((None, format!("❌ {image}: {err}")));
    }
    let status = info.status.clone().unwrap_or_default();
    if status.is_empty() {
        return None;
    }
    match &info.id {
        Some(id) => {
            let line = match info.progress.as_deref() {
                Some(p) if !p.is_empty() => format!("{id}: {status} {p}"),
                _ => format!("{id}: {status}"),
            };
            Some((Some(id.clone()), line))
        }
        None => Some((None, status)),
    }
}

/// Spawn `docker compose -f <file> pull` in `dir`, streaming its raw terminal
/// output. `--ansi always` forces Compose's in-place progress rendering even
/// though we capture it through a pipe; the popup's [`Screen`] replays it.
pub fn spawn_compose_pull(
    dir: PathBuf,
    file_name: String,
    project: String,
) -> (UnboundedReceiver<PullEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let _ = tx.send(PullEvent::Bytes(
            format!("⏬ docker compose pull ({project})\r\n").into_bytes(),
        ));

        let mut child = match Command::new("docker")
            .args(["compose", "--ansi", "always", "-f", &file_name, "pull"])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(PullEvent::Bytes(
                    format!("❌ failed to start docker compose: {e}\r\n").into_bytes(),
                ));
                let _ = tx.send(PullEvent::Done {
                    ok: false,
                    summary: "failed to start".into(),
                });
                return;
            }
        };

        // Compose writes its progress to stderr; stdout is captured too in case
        // a plain build/pull line shows up there.
        let mut readers: Vec<JoinHandle<()>> = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(spawn_byte_reader(out, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(spawn_byte_reader(err, tx.clone()));
        }

        let status = child.wait().await;
        for r in readers {
            let _ = r.await;
        }

        let ok = status.map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(PullEvent::Done {
            ok,
            summary: if ok {
                "pull complete".into()
            } else {
                "finished with errors".into()
            },
        });
    });
    (rx, handle)
}

fn spawn_byte_reader<R>(mut reader: R, tx: UnboundedSender<PullEvent>) -> JoinHandle<()>
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(PullEvent::Bytes(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Screen;

    fn render(screen: &Screen) -> Vec<String> {
        screen
            .lines
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        let mut s = Screen::new();
        s.feed(b"Downloading 10%\rDownloading 80%");
        assert_eq!(render(&s), vec!["Downloading 80%"]);
    }

    #[test]
    fn shorter_overwrite_leaves_no_tail() {
        // Erase-to-end (\x1b[K) after the carriage return clears the leftover.
        let mut s = Screen::new();
        s.feed(b"Downloading 100%\rDone\x1b[K");
        assert_eq!(render(&s), vec!["Done"]);
    }

    #[test]
    fn cursor_up_rewrites_previous_block() {
        // Mimics compose: print a 2-line block, move the cursor up 2, rewrite.
        let mut s = Screen::new();
        s.feed(b" radarr Pulling\r\n jellyfin Pulling\r\n");
        s.feed(b"\x1b[2A radarr Pulled \x1b[K\r\n jellyfin Pulled \x1b[K\r\n");
        assert_eq!(render(&s), vec![" radarr Pulled", " jellyfin Pulled", ""]);
    }

    #[test]
    fn escape_split_across_feeds() {
        // A CSI sequence arriving in two chunks must still be honoured.
        let mut s = Screen::new();
        s.feed(b"abc\x1b[2"); // incomplete: "...[2"
        s.feed(b"Dxy"); // completes CSI 'D' (cursor left 2), then writes
        // "abc", cursor left 2 -> col 1, write "xy" over "bc"
        assert_eq!(render(&s), vec!["axy"]);
    }

    #[test]
    fn multibyte_split_across_feeds() {
        // The ✔ (U+2714, 3 bytes) is split between two reads.
        let bytes = " ✔ Image ok".as_bytes();
        let split = 2; // mid-way through the ✔
        let mut s = Screen::new();
        s.feed(&bytes[..split]);
        s.feed(&bytes[split..]);
        assert_eq!(render(&s), vec![" ✔ Image ok"]);
    }
}
