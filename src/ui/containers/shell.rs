use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::theme::Theme;

/* ---------------- types ---------------- */

#[derive(Clone, Copy)]
enum ShellKind { Sh, Bash }

pub enum ShellEvent {
    None,
    Close,
}

/* ---------------- ShellPopup ---------------- */

pub struct ShellPopup {
    id: String,
    name: String,
    theme: Theme,

    _opened_at: Instant,
    shell: ShellKind,

    // cwd & dynamic cd
    cwd: String,
    prev_cwd: Option<String>,
    home: Option<String>,

    // input / history
    input: String,
    history: Vec<String>,
    hist_idx: Option<usize>,

    // output + state
    lines: Vec<String>,
    running: bool,
    scroll: usize,

    // cd autocompletion
    completions: Vec<String>,
    comp_idx: usize,
}

impl ShellPopup {
    pub fn new(id: String, name: String, theme: Theme) -> Self {
        Self {
            id, name, theme,
            _opened_at: Instant::now(),
            shell: ShellKind::Sh,
            cwd: "/".into(),
            prev_cwd: None,
            home: None,

            input: String::new(),
            history: Vec::new(),
            hist_idx: None,

            lines: vec![
                "Tips: type `help` for help, `clear` to clear, `exit` to close; `cd` persists. (New: cd, cd -, cd ~, Tab for autocomplete)".into()
            ],
            running: false,
            scroll: 0,

            completions: Vec::new(),
            comp_idx: 0,
        }
    }

    /// Detect sh/bash and HOME
    pub fn detect_shell(&mut self) {
        let ok_sh = self.quick_exec("echo ok", ShellKind::Sh).is_ok();
        if ok_sh {
            self.shell = ShellKind::Sh;
        } else {
            let ok_bash = self.quick_exec("echo ok", ShellKind::Bash).is_ok();
            if ok_bash { self.shell = ShellKind::Bash; }
        }
        // HOME inside the container
        if let Ok(h) = self.exec_raw_in_container("printf %s \"$HOME\"") {
            let h = h.trim();
            if !h.is_empty() { self.home = Some(h.to_string()); }
        }
    }

    fn quick_exec(&self, cmd: &str, kind: ShellKind) -> Result<()> {
        let (interp, flag) = match kind { ShellKind::Sh => ("sh", "-lc"), ShellKind::Bash => ("bash", "-lc") };
        let status = Command::new("docker")
            .args(["exec", "-i", &self.id, interp, flag, cmd])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status()?;
        if status.success() { Ok(()) } else { Err(anyhow!("non-zero")) }
    }

    fn exec_raw_in_container(&self, script: &str) -> Result<String> {
        let (interp, flag) = match self.shell { ShellKind::Sh => ("sh", "-lc"), ShellKind::Bash => ("bash", "-lc") };
        let out = Command::new("docker")
            .args(["exec", "-i", &self.id, interp, flag, script])
            .output()?;
        let mut s = String::new();
        if !out.stdout.is_empty() { s.push_str(&String::from_utf8_lossy(&out.stdout)); }
        if !out.stderr.is_empty() { s.push_str(&String::from_utf8_lossy(&out.stderr)); }
        Ok(s)
    }

    fn exec_in_container(&self, user_cmd: &str) -> Result<String> {
        let (interp, flag) = match self.shell { ShellKind::Sh => ("sh", "-lc"), ShellKind::Bash => ("bash", "-lc") };
        let full = if self.cwd == "/" {
            user_cmd.to_string()
        } else {
            format!("cd {} && {}", escape_for_shell(&self.cwd), user_cmd)
        };
        let out = Command::new("docker")
            .args(["exec", "-i", &self.id, interp, flag, &full])
            .output()?;

        let mut s = String::new();
        if !out.stdout.is_empty() { s.push_str(&String::from_utf8_lossy(&out.stdout)); }
        if !out.stderr.is_empty() { s.push_str(&String::from_utf8_lossy(&out.stderr)); }
        Ok(s)
    }

    /* ---------------- command line ---------------- */

    fn exec_line(&mut self, raw: &str) -> Result<()> {
        let line = raw.trim();
        if line.is_empty() { return Ok(()); }

        // builtins
        if line == "exit" { return Err(anyhow!("__EXIT")); }
        if line == "clear" { self.lines.clear(); return Ok(()); }
        if line == "help" {
            self.lines.extend([
                "Builtins:",
                "  • cd <dir> (persists) | cd - | cd ~",
                "  • clear, help, exit",
                "Notes:",
                "  • Line-by-line shell (no ncurses/full-screen apps)",
                "  • History ↑/↓, scroll PgUp/PgDn, Tab autocomplete on cd",
            ].into_iter().map(|s| s.to_string()));
            return Ok(());
        }

        if line == "cd -" {
            if let Some(prev) = self.prev_cwd.take() {
                self.prev_cwd = Some(self.cwd.clone());
                self.cwd = prev;
            } else {
                self.lines.push("cd: previous directory not set".into());
            }
            self.lines.push(format!("(cwd -> {})", self.cwd));
            return Ok(());
        }

        if line == "cd ~" {
            let target = self.home.clone().unwrap_or_else(|| "/root".into());
            self.prev_cwd = Some(self.cwd.clone());
            self.cwd = target;
            self.lines.push(format!("(cwd -> {})", self.cwd));
            return Ok(());
        }

        if line == "cd" {
            let target = self.home.clone().unwrap_or_else(|| "/root".into());
            self.prev_cwd = Some(self.cwd.clone());
            self.cwd = target;
            self.lines.push(format!("(cwd -> {})", self.cwd));
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix("cd ") {
            let new = rest.trim();
            let mut candidate = if new.starts_with('/') {
                new.to_string()
            } else if new == "-" {
                self.prev_cwd.clone().unwrap_or_else(|| self.cwd.clone())
            } else if new == "~" {
                self.home.clone().unwrap_or_else(|| "/root".into())
            } else if self.cwd == "/" {
                format!("/{}", new)
            } else {
                format!("{}/{}", self.cwd, new)
            };

            // light path normalization
            candidate = simplify_path(&candidate);

            // validate in the container
            let check = format!("cd {} && pwd", escape_for_shell(&candidate));
            if self.exec_raw_in_container(&check).is_ok() {
                self.prev_cwd = Some(self.cwd.clone());
                self.cwd = candidate;
                self.lines.push(format!("(cwd -> {})", self.cwd));
            } else {
                self.lines.push(format!("cd: {}: no such file or directory", new));
            }
            return Ok(());
        }

        // normal execution
        let prompt = format!("{}:{}$ {}", self.name, self.cwd, line);
        self.lines.push(prompt);

        self.running = true;
        let res = self.exec_in_container(line);
        self.running = false;

        match res {
            Ok(out) => {
                if !out.is_empty() {
                    for l in out.replace("\r\n", "\n").split('\n') {
                        self.lines.push(l.to_string());
                    }
                }
            }
            Err(e) => self.lines.push(format!("error: {e}")),
        }
        Ok(())
    }

    /* ---------------- cd autocompletion ---------------- */

    fn handle_tab_completion(&mut self) {
        let s = self.input.trim_end();
        if !s.starts_with("cd ") { return; }

        let pref = s[3..].trim();
        let (base_dir, pat) = split_base_and_pattern(pref, self.home.as_deref());

        // cycle if we already have suggestions
        if !self.completions.is_empty() {
            let name = &self.completions[self.comp_idx % self.completions.len()];
            let completed = join_base_name(&base_dir, name);
            self.input = format!("cd {}/", completed.trim_start_matches("./"));
            self.comp_idx = (self.comp_idx + 1) % self.completions.len();
            return;
        }

        // compute suggestions
        // 1) cd into cwd, then into base_dir
        // 2) list only directories that match `pat*`
        let script = format!(
            "cd {} 2>/dev/null || exit 0; \
             cd {} 2>/dev/null || exit 0; \
             for d in {}*; do [ -d \"$d\" ] && printf '%s\\n' \"$d\"; done",
            escape_for_shell(&self.cwd),
            escape_for_shell(&base_dir),
            escape_for_shell(&pat)
        );

        let out = match self.exec_raw_in_container(&script) {
            Ok(s) => s,
            Err(_) => String::new(),
        };

        let mut items: Vec<String> = out
            .lines()
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();

        for it in &mut items {
            if it.starts_with("./") { *it = it.trim_start_matches("./").to_string(); }
        }
        if items.is_empty() { return; }
        items.sort();

        if items.len() == 1 {
            let completed = join_base_name(&base_dir, &items[0]);
            self.input = format!("cd {}/", completed.trim_start_matches("./"));
            return;
        }

        let lcp = longest_common_prefix(&items);
        self.completions = items;
        self.comp_idx = 0;

        if !lcp.is_empty() && lcp != pat {
            let new_pref = join_base_name(&base_dir, &lcp);
            self.input = format!("cd {}", new_pref.trim_start_matches("./"));
            self.lines.push(format!("> {}", self.completions.join("  ")));
            return;
        }

        // insert the first suggestion and display the list; next Tab will cycle
        let first = &self.completions[0];
        let completed = join_base_name(&base_dir, first);
        self.input = format!("cd {}/", completed.trim_start_matches("./"));
        self.comp_idx = 1 % self.completions.len();
        self.lines.push(format!("> {}", self.completions.join("  ")));
    }

    fn reset_completion(&mut self) {
        self.completions.clear();
        self.comp_idx = 0;
    }

    /* ---------------- input events ---------------- */

    pub fn on_key(&mut self, key: KeyEvent) -> Result<ShellEvent> {
        match key.code {
            // close with Esc OR 'q'
            KeyCode::Esc | KeyCode::Char('q') => return Ok(ShellEvent::Close),

            KeyCode::Enter => {
                let line = self.input.clone();
                if !line.is_empty() {
                    if self.history.last().map(|s| s != &line).unwrap_or(true) {
                        self.history.push(line.clone());
                    }
                    self.hist_idx = None;
                }
                self.input.clear();
                self.reset_completion();

                match self.exec_line(&line) {
                    Ok(()) => {}
                    Err(e) if e.to_string() == "__EXIT" => return Ok(ShellEvent::Close),
                    Err(e) => self.lines.push(format!("error: {e}")),
                }
            }

            KeyCode::Backspace => { self.input.pop(); self.reset_completion(); }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear(); self.reset_completion();
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.lines.clear();
            }

            KeyCode::Char(ch) => { self.input.push(ch); self.reset_completion(); }

            KeyCode::Up => {
                self.reset_completion();
                if self.history.is_empty() { return Ok(ShellEvent::None); }
                let idx = match self.hist_idx {
                    None => self.history.len().saturating_sub(1),
                    Some(i) => i.saturating_sub(1),
                };
                self.hist_idx = Some(idx);
                if let Some(s) = self.history.get(idx) { self.input = s.clone(); }
            }
            KeyCode::Down => {
                self.reset_completion();
                if self.history.is_empty() { return Ok(ShellEvent::None); }
                match self.hist_idx {
                    None => {}
                    Some(i) if i + 1 < self.history.len() => {
                        let ni = i + 1;
                        self.hist_idx = Some(ni);
                        self.input = self.history[ni].clone();
                    }
                    _ => { self.hist_idx = None; self.input.clear(); }
                }
            }

            KeyCode::PageUp => {
                self.scroll = (self.scroll + 5).min(self.lines.len().saturating_sub(1));
            }
            KeyCode::PageDown => { self.scroll = self.scroll.saturating_sub(5); }

            KeyCode::Tab => { self.handle_tab_completion(); }
            KeyCode::BackTab => {
                if !self.completions.is_empty() {
                    if self.comp_idx == 0 { self.comp_idx = self.completions.len(); }
                    self.comp_idx -= 1;
                    let s = self.input.trim_end();
                    let pref = if s.starts_with("cd ") { s[3..].trim() } else { "" };
                    let (base_dir, _pat) = split_base_and_pattern(pref, self.home.as_deref());
                    let name = &self.completions[self.comp_idx % self.completions.len()];
                    let completed = join_base_name(&base_dir, name);
                    self.input = format!("cd {}/", completed.trim_start_matches("./"));
                }
            }

            _ => {}
        }
        Ok(ShellEvent::None)
    }

    /* ---------------- draw ---------------- */

    pub fn draw(&self, f: &mut Frame, area: Rect, tick: u64) {
        // overlay
        let w = (area.width * 4 / 5).max(60);
        let h = (area.height * 4 / 5).max(16);
        let overlay = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };

        f.render_widget(Clear, overlay);

        let title = format!("Shell — {} ({})", self.name, &self.id.chars().take(12).collect::<String>());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.theme.title(&title))
            .border_style(Style::default().fg(self.theme.accent));
        f.render_widget(block, overlay);

        let inner = Rect {
            x: overlay.x + 1,
            y: overlay.y + 1,
            width: overlay.width - 2,
            height: overlay.height - 2,
        };

        // layout: output area + input area + hints
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(inner.height.saturating_sub(4)),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        // output
        let mut lines: Vec<Line> = Vec::new();
        let spinner = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
        let spin = spinner[(tick as usize) % spinner.len()];
        let max_lines = chunks[0].height as usize;
        let total = self.lines.len();
        let start = total.saturating_sub(max_lines + self.scroll);
        let end = total.saturating_sub(self.scroll);
        for l in &self.lines[start..end] {
            lines.push(Line::raw(l.clone()));
        }
        if self.running && !lines.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("{spin} running…"),
                Style::default().fg(self.theme.accent),
            )));
        }
        let out = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        f.render_widget(out, chunks[0]);

        // input
        let prompt = format!("{}:{}$ ", self.name, self.cwd);
        let inp = Line::from(vec![
            Span::styled(
                prompt,
                Style::default()
                    .fg(self.theme.muted)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(&self.input),
            Span::styled("█", Style::default().fg(self.theme.accent)),
        ]);
        f.render_widget(Paragraph::new(inp), chunks[1]);

        // hints
        let sh = match self.shell { ShellKind::Sh => "sh", ShellKind::Bash => "bash" };
        let hint = Line::from(vec![
            Span::styled(
                "enter: run  |  esc/q: close  |  ↑/↓: history  |  PgUp/PgDn: scroll  |  ctrl-l: clear  |  Tab/⇧Tab: cd autocomplete",
                Style::default().fg(self.theme.muted),
            ),
            Span::raw("   "),
            Span::styled(
                format!("(cd, cd -, cd ~)  ({})", sh),
                Style::default().fg(Color::Rgb(120, 160, 255)),
            ),
        ]);
        f.render_widget(Paragraph::new(hint), chunks[2]);
    }
}

/* ---------------- helpers ---------------- */

/// Escape a simple argument for sh/bash
fn escape_for_shell(s: &str) -> String {
    if s.is_empty() { return "''".into(); }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        return s.into();
    }
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' { out.push_str("'\"'\"'"); } else { out.push(ch); }
    }
    out.push('\'');
    out
}

/// Joins base + name while handling "/" and "." nicely
fn join_base_name(base: &str, name: &str) -> String {
    if base == "." {
        name.to_string()
    } else if base == "/" {
        format!("/{}", name.trim_start_matches('/'))
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name.trim_start_matches('/'))
    }
}

/// Split a `cd ARG` into (base_dir, pattern)
fn split_base_and_pattern(arg: &str, home: Option<&str>) -> (String, String) {
    let arg = arg.trim();
    if arg.is_empty() { return (".".into(), String::new()); }
    let expanded = if arg == "~" {
        home.unwrap_or("/root").to_string()
    } else if arg.starts_with("~/") {
        format!("{}/{}", home.unwrap_or("/root"), arg.trim_start_matches("~/"))
    } else {
        arg.to_string()
    };
    if expanded.ends_with('/') {
        (expanded, String::new())
    } else {
        match expanded.rsplit_once('/') {
            Some((base, pat)) if !base.is_empty() => (base.to_string(), pat.to_string()),
            _ => (".".into(), expanded),
        }
    }
}

/// Longest common prefix across a set of strings
fn longest_common_prefix(items: &[String]) -> String {
    if items.is_empty() { return String::new(); }
    let mut it = items.iter();
    let mut prefix = it.next().unwrap().clone();
    for s in it {
        let max = prefix.len().min(s.len());
        let mut k = 0usize;
        while k < max && prefix.as_bytes()[k] == s.as_bytes()[k] { k += 1; }
        prefix.truncate(k);
        if prefix.is_empty() { break; }
    }
    prefix
}

/// Very simple path normalization (.., .)
fn simplify_path(p: &str) -> String {
    if p.is_empty() { return "/".into(); }
    let abs = p.starts_with('/');
    let parts = p.split('/').filter(|s| !s.is_empty());
    let mut stack: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "." => {}
            ".." => { stack.pop(); }
            _ => stack.push(part),
        }
    }
    let mut out = if abs { String::from("/") } else { String::new() };
    out.push_str(&stack.join("/"));
    if out.is_empty() { out.push('/'); }
    out
}
