use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::{TERMINAL_MIN_HEIGHT, TERMINAL_MIN_WIDTH};
use crate::theme::Theme;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/* ====================== Event API ====================== */

pub enum SplashEvent {
    Step { pct: f32, label: String },
    Done,
    Fail(String),
}

/* ====================== Splash Screen ====================== */

pub struct SplashScreen {
    theme: Theme,
    rx: UnboundedReceiver<SplashEvent>,

    tick: u64,
    progress: f32,
    target_progress: f32,
    label: String,

    done: bool,
    done_at: Option<Instant>,
    fail: Option<String>,
    skip_requested: bool,

    last_bar_tick: Instant,
    star_seed: u64,
}

impl SplashScreen {
    pub fn with_channel(theme: Theme) -> (Self, UnboundedSender<SplashEvent>) {
        let (tx, rx) = mpsc::unbounded_channel::<SplashEvent>();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5B1A5B);

        (
            Self {
                theme,
                rx,
                tick: 0,
                progress: 0.0,
                target_progress: 0.0,
                label: "initialisation…".into(),
                done: false,
                done_at: None,
                fail: None,
                skip_requested: false,
                last_bar_tick: Instant::now(),
                star_seed: seed,
            },
            tx,
        )
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.skip_requested = true,
            _ => {}
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        // initialization events
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                SplashEvent::Step { pct, label } => {
                    self.target_progress = pct.clamp(0.0, 1.0);
                    self.label = label;
                }
                SplashEvent::Done => {
                    self.target_progress = 1.0;
                    self.done = true;
                    self.done_at = Some(Instant::now());
                }
                SplashEvent::Fail(msg) => {
                    self.fail = Some(msg);
                }
            }
        }

        // smooth progress bar
        let dt = self.last_bar_tick.elapsed().as_secs_f32();
        self.last_bar_tick = Instant::now();
        let speed = 0.35; // ~35%/s
        let diff = self.target_progress - self.progress;
        let step = speed * dt;
        if diff.abs() <= step {
            self.progress = self.target_progress;
        } else {
            self.progress += step * diff.signum();
        }
    }

    pub fn is_ready_to_close(&self) -> bool {
        if self.fail.is_some() {
            return false;
        }
        if !self.done {
            return false;
        }
        let bar_full = self.progress >= 0.999;
        let hold_ok = self
            .done_at
            .map(|t| t.elapsed() >= Duration::from_millis(400))
            .unwrap_or(false);
        bar_full && (hold_ok || self.skip_requested)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        // centered wide container
        let w = (area.width * 4 / 5).max(TERMINAL_MIN_WIDTH);
        let h = (area.height * 4 / 5).max(TERMINAL_MIN_HEIGHT);
        let overlay = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };

        f.render_widget(Clear, overlay);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(self.theme.title(" dockrtui "));
        f.render_widget(block, overlay);

        // inner
        let inner = Rect {
            x: overlay.x + 1,
            y: overlay.y + 1,
            width: overlay.width.saturating_sub(2),
            height: overlay.height.saturating_sub(2),
        };

        // layout: large space area + bottom (progress + footer)
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(h.saturating_sub(9)), // SPACE + CENTERED BANNER
                Constraint::Length(5),                // PROGRESS
                Constraint::Length(2),                // FOOTER
            ])
            .split(inner);

        // 1) Animated space (stars + nebula + meteor)
        let space_lines = render_space_scene(
            rows[0].width as usize,
            rows[0].height as usize,
            self.tick,
            self.star_seed,
            self.theme,
        );
        let space = Paragraph::new(Text::from(space_lines)).wrap(Wrap { trim: false });
        f.render_widget(space, rows[0]);

        // 2) Giant rainbow title — rendered OVER the space, centered
        let banner = render_big_banner("DOCKRTUI", rows[0].width as usize, self.tick);
        let banner_height = banner.len() as u16;
        let banner_rect = Rect {
            x: rows[0].x,
            y: rows[0].y + rows[0].height.saturating_sub(banner_height) / 2,
            width: rows[0].width,
            height: banner_height,
        };
        let banner_par = Paragraph::new(Text::from(banner)).alignment(Alignment::Center);
        f.render_widget(banner_par, banner_rect);

        // Version in small text (top right corner of global rectangle)
        {
            let v = format!("v{}", VERSION);
            let mut line = Line::default();
            line.spans.push(Span::styled(
                v,
                Style::default()
                    .fg(self.theme.muted)
                    .add_modifier(Modifier::DIM),
            ));
            let rect = Rect {
                x: overlay.x + overlay.width.saturating_sub(8),
                y: overlay.y,
                width: 8,
                height: 1,
            };
            f.render_widget(Paragraph::new(line), rect);
        }

        // 3) Progress bar + label/error
        let pb = fancy_bar_line(
            rows[1].width as usize,
            self.progress,
            self.tick,
            self.theme.accent,
            self.theme.accent_alt,
        );
        let bar = Paragraph::new(pb).block(
            Block::default()
                .borders(Borders::ALL)
                .title("initialisation"),
        );
        f.render_widget(bar, rows[1]);

        // 4) Hints / errors
        let mut foot = Vec::<Span>::new();
        match &self.fail {
            Some(err) => {
                foot.push(Span::styled(
                    " init failed: ",
                    Style::default()
                        .fg(self.theme.err)
                        .add_modifier(Modifier::BOLD),
                ));
                foot.push(Span::styled(err, Style::default().fg(self.theme.err)));
                foot.push(Span::raw("  • press q to quit"));
            }
            None => {
                foot.push(Span::styled(
                    format!(" step: {}", self.label),
                    Style::default().fg(self.theme.fg),
                ));
                foot.push(Span::raw("  • esc/enter: skip  • q: quit"));
            }
        }
        let foot = Paragraph::new(Line::from(foot)).style(Style::default().fg(self.theme.muted));
        f.render_widget(foot, rows[2]);
    }
}

/* ====================== BANNER Rendering (8 lines) ====================== */

/// 8x8 glyphs for D,O,C,K,R,T,U,I (sufficient for "DOCKRTUI")
fn glyph8(ch: char) -> &'static [&'static str] {
    match ch.to_ascii_uppercase() {
        'D' => &[
            "██████  ",
            "██   ██ ",
            "██    ██",
            "██    ██",
            "██    ██",
            "██   ██ ",
            "██████  ",
            "        ",
        ],
        'O' => &[
            " █████  ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            " █████  ",
            "        ",
        ],
        'C' => &[
            " ██████ ",
            "██      ",
            "██      ",
            "██      ",
            "██      ",
            "██      ",
            " ██████ ",
            "        ",
        ],
        'K' => &[
            "██  ██  ",
            "██ ██   ",
            "████    ",
            "███     ",
            "████    ",
            "██ ██   ",
            "██  ██  ",
            "        ",
        ],
        'R' => &[
            "██████  ",
            "██   ██ ",
            "██████  ",
            "██  ██  ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "        ",
        ],
        'T' => &[
            "████████",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "        ",
        ],
        'U' => &[
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            "██   ██ ",
            " █████  ",
            "        ",
        ],
        'I' => &[
            "████████",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "   ██   ",
            "████████",
            "        ",
        ],
        _ => &[
            "        ", "        ", "        ", "        ", "        ", "        ", "        ",
            "        ",
        ],
    }
}

fn render_big_banner(text: &str, max_w: usize, tick: u64) -> Vec<Line<'static>> {
    let spacing = 2usize;
    let lines = 8usize;
    let glyph_w = 8usize;

    let chars = text.chars().collect::<Vec<_>>();
    let total_w = chars.len() * glyph_w + (chars.len().saturating_sub(1)) * spacing;

    // animated rainbow phase
    let phase = (tick as f32 / 70.0) % 1.0;

    let mut out: Vec<Line> = Vec::with_capacity(lines);
    for row in 0..lines {
        let mut spans: Vec<Span> = Vec::with_capacity(total_w + 8);
        let mut col_x = 0usize;

        for (i, ch) in chars.iter().enumerate() {
            let g = glyph8(*ch)[row];
            for (j, c) in g.chars().enumerate() {
                let x = col_x + j;
                if x >= max_w {
                    break;
                }

                // normalized position over total width for gradient
                let t = if total_w <= 1 {
                    0.0
                } else {
                    x as f32 / (total_w - 1) as f32
                };
                // "wave" of brightness sweeping across
                let wave = ((t * 6.0 + row as f32 * 0.6 + phase * std::f32::consts::TAU).sin()
                    * 0.5
                    + 0.5)
                    .clamp(0.0, 1.0);
                let hue = (360.0 * (t + phase)).rem_euclid(360.0);
                let col = hsv_to_rgb(hue, 0.9, 0.55 + 0.45 * wave);

                let s = if c == ' ' {
                    Span::raw(" ")
                } else {
                    Span::styled("█", Style::default().fg(col).add_modifier(Modifier::BOLD))
                };
                spans.push(s);
            }
            col_x += glyph_w;

            // spacing between glyphs
            if i + 1 != chars.len() {
                for _ in 0..spacing {
                    if col_x >= max_w {
                        break;
                    }
                    spans.push(Span::raw(" "));
                    col_x += 1;
                }
            }

            if col_x >= max_w {
                break;
            }
        }

        out.push(Line::from(spans));
    }
    out
}

/* ====================== Animated Space ====================== */

fn render_space_scene(
    w: usize,
    h: usize,
    tick: u64,
    mut seed: u64,
    _theme: Theme,
) -> Vec<Line<'static>> {
    // densité étoiles & nébuleuses
    let star_count = (w * h / 42).max(20);
    let nebula_bands = 3;

    // buffer “plain” (on stylise à la volée)
    let mut rows: Vec<Vec<(char, Option<Color>)>> = vec![vec![(' ', None); w]; h];

    // 1) Nappes nébuleuses (diagonales colorées)
    for b in 0..nebula_bands {
        let t = (tick as f32 / 90.0) + b as f32 * 0.8;
        let hue_base = (360.0 * ((b as f32 * 0.23 + t * 0.03) % 1.0)).rem_euclid(360.0);
        let col = hsv_to_rgb(hue_base, 0.55, 0.22);
        // bande diagonale sinusoïdale
        for y in 0..h {
            let fy = y as f32 / h.max(1) as f32;
            let cx = ((fy + (b as f32) * 0.18 + t * 0.02) * std::f32::consts::TAU).sin();
            let mid = ((w as f32) * (0.5 + 0.4 * cx)) as isize;
            let thickness = (3 + (3.0 * (fy * std::f32::consts::PI).sin().abs()) as usize) as isize;
            for dx in -thickness..=thickness {
                let x = (mid + dx).clamp(0, w as isize - 1) as usize;
                if rows[y][x].0 == ' ' {
                    rows[y][x] = ('▒', Some(col));
                }
            }
        }
    }

    // 2) étoiles (scintillement)
    for _ in 0..star_count {
        seed = lcg(seed);
        let x = (seed % (w as u64)) as usize;
        seed = lcg(seed);
        let y = (seed % (h as u64)) as usize;

        // twinkle cadence
        let tw = ((tick + ((x as u64) << 1) + (y as u64)) % 18) as usize;
        let ch = match tw {
            0..=2 => '·',
            3..=8 => '•',
            9..=14 => '✦',
            _ => '•',
        };

        // quelques étoiles colorées
        let colored = (x.wrapping_mul(1315423911) ^ y.wrapping_mul(2654435761)) % 7 == 0;
        let col = if colored {
            let hue =
                ((x as f32 / (w.max(1) as f32) + (tick as f32) * 0.002).fract() * 360.0) % 360.0;
            Some(hsv_to_rgb(hue, 0.7, 0.9))
        } else {
            Some(Color::Rgb(200, 200, 220))
        };

        rows[y][x] = (ch, col);
    }

    // 3) petit météore (très lisible)
    if w > 12 && h > 3 {
        let period = 160;
        if tick % period < 24 {
            let t = (tick % period) as f32 / 24.0;
            let mx = ((w as f32) * (0.9 - 0.9 * t))
                .round()
                .clamp(0.0, (w - 1) as f32) as usize;
            let my = (h as f32 * (0.15 + 0.7 * t)) as usize % h;
            let c_head = hsv_to_rgb(20.0 + 340.0 * t, 0.9, 1.0);
            rows[my][mx] = ('✱', Some(c_head));
            if mx + 1 < w {
                rows[my][mx + 1] = ('•', Some(Color::Rgb(255, 220, 200)));
            }
            if mx + 2 < w {
                rows[my][mx + 2] = ('·', Some(Color::Rgb(220, 180, 160)));
            }
        }
    }

    // conversion en Lines (groupage naïf)
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::with_capacity(w / 2);
            let mut cur_style: Option<Color> = None;
            let mut buf = String::new();

            let flush = |spans: &mut Vec<Span>, buf: &mut String, style: &Option<Color>| {
                if buf.is_empty() {
                    return;
                }
                match style {
                    Some(c) => spans.push(Span::styled(buf.clone(), Style::default().fg(*c))),
                    None => spans.push(Span::raw(buf.clone())),
                }
                buf.clear();
            };

            for (ch, col) in row {
                if col != cur_style {
                    flush(&mut spans, &mut buf, &cur_style);
                    cur_style = col;
                }
                buf.push(ch);
            }
            flush(&mut spans, &mut buf, &cur_style);
            Line::from(spans)
        })
        .collect()
}

/* ====================== Helpers Couleur & Progress ====================== */

fn lcg(x: u64) -> u64 {
    // Linear Congruential Generator
    x.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Color {
    // h: 0..360, s/v: 0..1
    let c = v * s;
    let hp = (h / 60.0) % 6.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to_u8 = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(to_u8(r), to_u8(g), to_u8(b))
}

fn fancy_bar_line(width: usize, progress: f32, tick: u64, c1: Color, c2: Color) -> Line<'static> {
    let width = width.saturating_sub(2);
    let pct = (progress * 100.0).round() as i32;
    let fill = (progress * width as f32).floor() as usize;
    let shine = if width == 0 {
        0
    } else {
        (tick as usize / 2) % width
    };

    let mix = |a: Color, b: Color, t: f32| -> Color {
        match (a, b) {
            (Color::Rgb(ra, ga, ba), Color::Rgb(rb, gb, bb)) => {
                let lerp = |x, y| {
                    (x as f32 + (y as f32 - x as f32) * t)
                        .round()
                        .clamp(0.0, 255.0) as u8
                };
                Color::Rgb(lerp(ra, rb), lerp(ga, gb), lerp(ba, bb))
            }
            _ => a,
        }
    };

    let mut spans: Vec<Span> = Vec::with_capacity(width + 10);
    spans.push(Span::raw(" "));

    for i in 0..width {
        let t = if width <= 1 {
            0.0
        } else {
            i as f32 / (width - 1) as f32
        };
        let base = mix(c1, c2, t);

        let shine_dist = (i as isize - shine as isize).abs() as usize;
        let shine_boost = (3usize.saturating_sub(shine_dist)) as f32 / 3.0;

        let ch = if i < fill { '█' } else { '░' };
        let mut style = if i < fill {
            Style::default().fg(base).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(40, 42, 46))
        };
        if shine_boost > 0.0 && i < fill {
            style = Style::default()
                .fg(mix(base, Color::Rgb(255, 255, 255), shine_boost * 0.6))
                .add_modifier(Modifier::BOLD);
        }

        spans.push(Span::styled(ch.to_string(), style));
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!(" {}% ", pct),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    Line::from(spans)
}
