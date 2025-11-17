use rand::{Rng, SeedableRng, rngs::StdRng};

use super::actions::ActionKind;

#[inline]
pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}
#[inline]
pub fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

pub fn rocket_up_from_bar(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    if p < 0.15 {
        0.05 * ease_out_cubic(p / 0.15)
    } else if p < 0.60 {
        0.05 + 0.60 * ease_out_cubic((p - 0.15) / 0.45)
    } else if p < 0.98 {
        0.65 + 0.25 * ease_in_out_cubic((p - 0.60) / 0.38)
    } else {
        0.90 + 0.10 * ((p - 0.98) / 0.02).clamp(0.0, 1.0)
    }
}

pub fn rocket_down_from_bar(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    if p < 0.20 {
        0.05 * ease_out_cubic(p / 0.20)
    } else if p < 0.70 {
        0.05 + 0.60 * ease_in_out_cubic((p - 0.20) / 0.50)
    } else if p < 0.98 {
        0.65 + 0.30 * ease_in_out_cubic((p - 0.70) / 0.28)
    } else {
        0.95 + 0.05 * ((p - 0.98) / 0.02).clamp(0.0, 1.0)
    }
}

/// Rendu ASCII de la scène verticale (fusée + fond)
pub fn render_rocket_scene_vertical(
    kind: ActionKind,
    w: u16,
    h: u16,
    progress: f32,
    tick: u64,
) -> String {
    let w = w as usize;
    let h = h as usize;
    if w == 0 || h == 0 {
        return String::new();
    }

    let mut buf = vec![vec![' '; w]; h];

    // Sol
    let ground_y = h.saturating_sub(2);
    for x in 0..w {
        buf[ground_y][x] = if x % 2 == 0 { '‾' } else { ' ' };
    }
    for x in 0..w {
        if x % 4 == 0 {
            buf[ground_y.saturating_sub(1)][x] = '.';
        }
    }

    // Parallaxe simple
    let parallax_shift = match kind {
        ActionKind::Starting => (progress * h as f32 * 0.35) as isize,
        ActionKind::Stopping => -(progress * h as f32 * 0.35) as isize,
    };
    let layers = [
        (22usize, 10u64, '·', 0.35f32),
        (34, 6, '˙', 0.65f32),
        (52, 4, '·', 1.00f32),
    ];
    for (den, speed, ch, par) in layers {
        let mut rng = StdRng::seed_from_u64(0x51DE + speed);
        let count = (w * h / den).max(10);
        let dy = ((parallax_shift as f32 * par).round() as isize
            + (tick as isize / (speed as isize * 5)))
            .clamp(-10000, 10000);
        for _ in 0..count {
            let x0 = rng.random_range(0..w) as isize;
            let y0 = rng.random_range(0..h) as isize;
            let x = x0.rem_euclid(w as isize) as usize;
            let y = (y0 + dy).rem_euclid(h as isize) as usize;
            if y < ground_y {
                buf[y][x] = ch;
            }
        }
    }

    // Fusée
    const ROCKET: &[&str] = &[
        "   /\\   ",
        "  /  \\  ",
        " /++++\\ ",
        " | ++ | ",
        " | ++ | ",
        " |____| ",
        "/|####|\\",
        "| |##| |",
        " \\====/ ",
        "   ||   ",
        "   ||   ",
    ];
    let rh = ROCKET.len();
    let rw = ROCKET[0].chars().count();

    let start_on_ground = ground_y.saturating_sub(rh);
    let off_extra_top: isize = ((h as isize) / 2).max(8);
    let end_off_top: isize = -((rh as isize) + off_extra_top);
    let pe = ease_in_out_cubic(progress);

    let y_world: isize = match kind {
        ActionKind::Starting => {
            (start_on_ground as isize)
                + ((end_off_top - start_on_ground as isize) as f32 * pe) as isize
        }
        ActionKind::Stopping => {
            (end_off_top as isize)
                + ((start_on_ground as isize - end_off_top as isize) as f32 * pe) as isize
        }
    };

    // Wobble
    let wobble = {
        let amp = if matches!(kind, ActionKind::Starting) {
            1.0 - progress
        } else {
            progress
        };
        (amp * 1.5 * (tick as f32 / 7.0).sin()).round() as isize
    };
    let x_center = (w as isize / 2) + wobble;
    let x_left = (x_center - (rw as isize / 2)).clamp(0, (w.saturating_sub(rw)) as isize) as usize;

    // Dessin fusée
    for (i, line) in ROCKET.iter().enumerate() {
        let y = y_world + i as isize;
        if y < 0 || (y as usize) >= h {
            continue;
        }
        let yy = y as usize;
        for (j, ch) in line.chars().enumerate() {
            let xx = x_left + j;
            if xx < w && yy < ground_y {
                buf[yy][xx] = ch;
            }
        }
    }

    // Flamme
    let throttle = match kind {
        ActionKind::Starting => (0.40 + 0.60 * progress).clamp(0.4, 1.0),
        ActionKind::Stopping => (0.15 + 0.25 * (1.0 - progress)).clamp(0.12, 0.4),
    };
    let flame_h = (3.0 + 7.0 * throttle).round() as usize;
    let flicker = ((tick / 2) % 3) as usize;

    let base_y = (y_world + rh as isize) as isize;
    for dy in 0..(flame_h as isize) {
        let y = base_y + dy;
        if y < 0 || (y as usize) >= ground_y {
            continue;
        }
        for dx in [-2isize, 0, 2] {
            let jitter = (((tick + (dy as u64) + (dx.abs() as u64)) % 3) as isize - 1) as isize;
            let xx = (x_center + dx + jitter).clamp(0, (w - 1) as isize) as usize;
            let ch = match ((dy as usize) + flicker) % 4 {
                0 => '╽',
                1 => '│',
                2 => '╿',
                _ => '│',
            };
            buf[y as usize][xx] = ch;
        }
    }

    // Fumée montée (décollage)
    if matches!(kind, ActionKind::Starting) {
        let near_ground = ((base_y + 2) >= ground_y as isize) as i32;
        if near_ground == 1 {
            let mut rng = StdRng::seed_from_u64(0xA115A10CE + tick);
            let puffs = (6.0 + 10.0 * (1.0 - progress)).round() as usize;
            for _ in 0..puffs {
                let dx = rng.random_range(-3i32..=3i32) as isize;
                let mut y = ground_y.saturating_sub(1) as isize;
                y = y.saturating_sub(((tick % 6) / 3) as isize);
                let xx = (x_center + dx).clamp(0, (w - 1) as isize) as usize;
                if y >= 1 && (y as usize) < ground_y {
                    buf[y as usize][xx] = if (tick + xx as u64) % 2 == 0 {
                        '·'
                    } else {
                        ','
                    };
                }
            }
        }
    }

    // Poussière (atterrissage)
    if matches!(kind, ActionKind::Stopping) && base_y + 1 >= ground_y as isize {
        let spread = (10.0 + 22.0 * progress).round() as usize;
        let start = x_left.saturating_sub(spread / 2);
        let end = (x_left + rw + spread / 2).min(w);
        for x in start..end {
            if (x + tick as usize) % 2 == 0 {
                let y = ground_y.saturating_sub(1);
                buf[y][x] = if (x + (tick as usize / 2)) % 3 == 0 {
                    '~'
                } else {
                    '='
                };
            }
        }
    }

    buf.into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
