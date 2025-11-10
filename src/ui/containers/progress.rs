use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn mix(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ra, ga, ba), Color::Rgb(rb, gb, bb)) => {
            let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
            Color::Rgb(lerp(ra, rb), lerp(ga, gb), lerp(ba, bb))
        }
        _ => a,
    }
}

/// Progress bar with "shine" effect — returns a `Line`
pub fn fancy_bar_line(width: usize, progress: f32, tick: u64, c1: Color, c2: Color) -> Line<'static> {
    let width = width.saturating_sub(2);
    let pct = (progress * 100.0).round() as i32;
    let fill = (progress * width as f32).floor() as usize;
    let shine = if width == 0 { 0 } else { (tick as usize / 2) % width };

    let mut spans: Vec<Span> = Vec::with_capacity(width + 10);
    spans.push(Span::styled(" ", Style::default())); // margin

    for i in 0..width {
        let t = if width <= 1 { 0.0 } else { i as f32 / (width - 1) as f32 };
        let base = mix(c1, c2, t);
        let shine_dist = (i as isize - shine as isize).abs() as usize;
        let shine_boost = (3usize.saturating_sub(shine_dist)) as f32 / 3.0;

        let ch = if i < fill { '█' } else { '░' };
        let style = if i < fill {
            Style::default().fg(base).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(40, 42, 46))
        };
        let style = if shine_boost > 0.0 && i < fill {
            Style::default()
                .fg(mix(base, Color::Rgb(255, 255, 255), shine_boost * 0.6))
                .add_modifier(Modifier::BOLD)
        } else { style };

        spans.push(Span::styled(ch.to_string(), style));
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled(format!(" {}% ", pct), Style::default().add_modifier(Modifier::BOLD)));

    Line::from(spans)
}
