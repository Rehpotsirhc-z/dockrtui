use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::theme::Theme;

pub fn selected_row_style(theme: Theme, tick: u64) -> Style {
    let breath = ((tick % 40) as f32 / 40.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    let sel_bg = lerp(theme.accent_alt, theme.accent, breath);
    Style::default()
        .bg(sel_bg)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD)
}

pub fn alt_row_style() -> Style {
    Style::default().bg(Color::Rgb(16, 18, 20))
}

pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let to_u8 = |x: f32| (x.round() as i32).clamp(0, 255) as u8;
    let clamp = |x: f32| x.clamp(0.0, 255.0);
    match (a, b) {
        (Color::Rgb(ra, ga, ba), Color::Rgb(rb, gb, bb)) => {
            let r = clamp(ra as f32 + (rb as f32 - ra as f32) * t);
            let g = clamp(ga as f32 + (gb as f32 - ga as f32) * t);
            let bl = clamp(ba as f32 + (bb as f32 - ba as f32) * t);
            Color::Rgb(to_u8(r), to_u8(g), to_u8(bl))
        }
        _ => b,
    }
}

pub fn grad_sweep(text: &str, from: Color, to: Color, phase: f32) -> Line<'static> {
    let chars = text.chars().collect::<Vec<_>>();
    let n = (chars.len().max(1) - 1) as f32;
    let spans = chars
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let pos = i as f32 / n;
            let wave = ((pos - phase).abs() * 6.0).clamp(0.0, 1.0);
            let t = 1.0 - wave;
            Span::styled(ch.to_string(), Style::default().fg(lerp(from, to, t)))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Tronque une chaîne au milieu en gardant début + fin
pub fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return "...".into();
    }
    let keep = max - 3;
    let head = keep / 2;
    let tail = keep - head;
    let mut it = s.chars();
    let start: String = it.by_ref().take(head).collect();
    let end: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}...{end}")
}
