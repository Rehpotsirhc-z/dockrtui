use std::collections::VecDeque;
use std::time::Instant;

use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use serde_json::Value;

use crate::{docker::DockerClient, theme::Theme};

// ~1 minute of history if the tick is ~500ms
const CAPACITY: usize = 120;
const BLOCKS: [char; 8] = ['▁','▂','▃','▄','▅','▆','▇','█'];

#[derive(Copy, Clone, Debug)]
struct Sample {
    ts: Instant,
    cpu_pct: f64,       // raw CPU% from Docker (can be > 100)
    mem_used_b: f64,
    mem_limit_b: f64,
    rx_total_b: u64,
    tx_total_b: u64,
    cpu_total_raw: u64,
    _system_total_raw: u64,
}

pub struct StatsPane {
    docker: DockerClient,
    pub visible: bool,
    pub container_id: Option<String>,

    // smoothed time series (ready-to-display units)
    hist_cpu: VecDeque<(Instant, f64)>,     // %
    hist_mem_mib: VecDeque<(Instant, f64)>, // MiB
    hist_rx_bps: VecDeque<(Instant, f64)>,  // B/s
    hist_tx_bps: VecDeque<(Instant, f64)>,

    last: Option<Sample>,
    last_ncpu: u64,

    rx: Option<Receiver<Value>>,
    stop_tx: Option<Sender<()>>,
}

impl StatsPane {
    pub fn new(docker: DockerClient) -> Self {
        Self {
            docker,
            visible: false,
            container_id: None,
            hist_cpu: VecDeque::with_capacity(CAPACITY),
            hist_mem_mib: VecDeque::with_capacity(CAPACITY),
            hist_rx_bps: VecDeque::with_capacity(CAPACITY),
            hist_tx_bps: VecDeque::with_capacity(CAPACITY),
            last: None,
            last_ncpu: 1,
            rx: None,
            stop_tx: None,
        }
    }

    pub fn set_visible(&mut self, vis: bool) {
        if vis && !self.visible {
            self.visible = true;
            if self.rx.is_none() && self.container_id.is_some() {
                self.restart_stream();
            }
        } else if !vis && self.visible {
            self.visible = false;
            self.stop_stream();
        }
    }

    pub fn attach(&mut self, id: &str) {
        if self.container_id.as_deref() != Some(id) {
            self.container_id = Some(id.to_string());
            self.reset_series();
            if self.visible {
                self.restart_stream();
            }
        }
    }

    fn reset_series(&mut self) {
        self.hist_cpu.clear();
        self.hist_mem_mib.clear();
        self.hist_rx_bps.clear();
        self.hist_tx_bps.clear();
        self.last = None;
        self.last_ncpu = 1;
    }

    fn stop_stream(&mut self) {
        if let Some(tx) = self.stop_tx.take() { let _ = tx.try_send(()); }
        self.rx = None;
    }

    fn restart_stream(&mut self) {
        self.stop_stream();
        let Some(id) = self.container_id.clone() else { return; };
        let docker = self.docker.clone();

        let (tx_samples, rx_samples) = async_channel::bounded::<Value>(64);
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
        self.rx = Some(rx_samples);
        self.stop_tx = Some(stop_tx.clone());

        tokio::spawn(async move {
            match docker.stats_stream_live(&id).await {
                Ok(mut s) => {
                    loop {
                        tokio::select! {
                            _ = stop_rx.recv() => break,
                            item = s.next() => {
                                match item {
                                    Some(Ok(v)) => { let _ = tx_samples.try_send(v); }
                                    Some(Err(_)) | None => break,
                                }
                            }
                        }
                    }
                }
                Err(_) => {}
            }
        });
    }

    pub async fn on_tick(&mut self) {
        if !self.visible { return; }
        if self.rx.is_none() {
            if self.container_id.is_some() { self.restart_stream(); } else { return; }
        }

        // Coalescing: keep only the last sample for this tick
        let mut last_msg: Option<Value> = None;
        while let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(v) => { last_msg = Some(v); continue; }
                Err(async_channel::TryRecvError::Empty) => break,
                Err(async_channel::TryRecvError::Closed) => { self.rx = None; break; }
            }
        }
        if let Some(v) = last_msg { self.ingest(v); }

        trim(&mut self.hist_cpu, CAPACITY);
        trim(&mut self.hist_mem_mib, CAPACITY);
        trim(&mut self.hist_rx_bps, CAPACITY);
        trim(&mut self.hist_tx_bps, CAPACITY);
    }

    fn range_last(hist: &VecDeque<(Instant,f64)>, n: usize) -> (f64, f64) {
        if hist.is_empty() { return (0.0, 1.0); }
        let take = hist.len().min(n);
        let start = hist.len() - take;
        let mut mn = f64::INFINITY;
        let mut mx = f64::NEG_INFINITY;
        for &(_,y) in hist.iter().skip(start) {
            if y < mn { mn = y; }
            if y > mx { mx = y; }
        }
        if !mn.is_finite() || !mx.is_finite() || mn==mx { (0.0, 1.0) } else { (mn, mx) }
    }

    fn ingest(&mut self, v: Value) {
        let now = Instant::now();

        // ncpu
        self.last_ncpu = v.pointer("/cpu_stats/online_cpus")
            .and_then(|x| x.as_u64())
            .or_else(|| v.pointer("/cpu_stats/cpu_usage/percpu_usage").and_then(|x| x.as_array()).map(|a| a.len() as u64))
            .unwrap_or(1)
            .max(1);

        // memory
        let usage = pointer_u64(&v, "/memory_stats/usage") as f64;
        let limit = pointer_u64(&v, "/memory_stats/limit") as f64;
        let inactive_file = v.pointer("/memory_stats/stats/inactive_file").and_then(|x| x.as_u64()).unwrap_or(0) as f64;
        let cache = v.pointer("/memory_stats/stats/cache").and_then(|x| x.as_u64()).unwrap_or(0) as f64;
        // try to remove cached pages to get "real" memory usage
        let mem_used_b = if inactive_file > 0.0 { (usage - inactive_file).max(0.0) }
                         else if cache > 0.0 { (usage - cache).max(0.0) }
                         else { usage };

        // CPU
        let cpu_total = pointer_u64(&v, "/cpu_stats/cpu_usage/total_usage");
        let pre_total = pointer_u64(&v, "/precpu_stats/cpu_usage/total_usage");
        let system_total = pointer_u64(&v, "/cpu_stats/system_cpu_usage");
        let pre_system = pointer_u64(&v, "/precpu_stats/system_cpu_usage");

        let mut cpu_pct = if pre_total > 0 && pre_system > 0 && system_total > 0 {
            // mode 1: % of host time slice * ncpu (can be > 100)
            let cpu_delta = (cpu_total as f64) - (pre_total as f64);
            let sys_delta = (system_total as f64) - (pre_system as f64);
            if sys_delta > 0.0 { (cpu_delta / sys_delta) * (self.last_ncpu as f64) * 100.0 } else { 0.0 }
        } else if let Some(prev) = self.last {
            // fallback: derive from real elapsed time
            let dt = (now - prev.ts).as_secs_f64().max(1e-6);
            let d_total = cpu_total.saturating_sub(prev.cpu_total_raw) as f64;
            let cap_ns = (self.last_ncpu as f64) * dt * 1_000_000_000.0;
            if cap_ns > 0.0 { (d_total / cap_ns) * 100.0 } else { 0.0 }
        } else { 0.0 };

        if let Some(prev) = self.last {
            // if Docker keeps returning the same total, keep last known % to avoid flicker
            if cpu_pct <= 0.0 && cpu_total == prev.cpu_total_raw { cpu_pct = prev.cpu_pct; }
        }
        // keep the real value (may be >100), we only normalize for display
        cpu_pct = cpu_pct.max(0.0);

        // Network: totals -> B/s
        let (rx_tot, tx_tot) = net_totals_bytes(&v);
        let (rx_bps, tx_bps) = if let Some(prev) = self.last {
            let dt = (now - prev.ts).as_secs_f64().max(1e-6);
            (
                (rx_tot.saturating_sub(prev.rx_total_b)) as f64 / dt,
                (tx_tot.saturating_sub(prev.tx_total_b)) as f64 / dt,
            )
        } else { (0.0, 0.0) };

        // Simple EMA smoothing for nicer visuals
        let ema = |last: Option<f64>, x: f64, a: f64| -> f64 {
            if let Some(l) = last { l * (1.0 - a) + x * a } else { x }
        };
        let last_cpu = self.hist_cpu.back().map(|&(_,v)| v);
        let last_mem = self.hist_mem_mib.back().map(|&(_,v)| v);
        let last_rx  = self.hist_rx_bps.back().map(|&(_,v)| v);
        let last_tx  = self.hist_tx_bps.back().map(|&(_,v)| v);

        let cpu_s = ema(last_cpu, cpu_pct, 0.35);                                  // %
        let mem_s = ema(last_mem, mem_used_b / (1024.0 * 1024.0), 0.25);           // MiB
        let rx_s  = ema(last_rx,  rx_bps, 0.35);                                    // B/s
        let tx_s  = ema(last_tx,  tx_bps, 0.35);

        self.hist_cpu.push_back((now, cpu_s));
        self.hist_mem_mib.push_back((now, mem_s));
        self.hist_rx_bps.push_back((now, rx_s));
        self.hist_tx_bps.push_back((now, tx_s));

        self.last = Some(Sample {
            ts: now,
            cpu_pct,
            mem_used_b,
            mem_limit_b: limit,
            rx_total_b: rx_tot,
            tx_total_b: tx_tot,
            cpu_total_raw: cpu_total,
            _system_total_raw: system_total,
        });
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, theme: Theme) {
        if !self.visible { return; }
        if area.height < 10 || area.width < 30 { return; }

        // Centered overlay, slightly inset (98% × 86% of the available area)
        let pane = centered_rect(98, 86, area);

        f.render_widget(Clear, pane);
        let block = Block::default().borders(Borders::ALL).title(theme.title("Stats"));
        f.render_widget(block, pane);

        let inner = pad(pane, 1);
        // Give more space to CPU/MEM (60% / 40%)
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(inner);

        let cols1 = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        let cols2 = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);

        self.draw_cpu_card(f, cols1[0], theme);
        self.draw_mem_card(f, cols1[1], theme);
        self.draw_net_card(f, cols2[0], theme, true);
        self.draw_net_card(f, cols2[1], theme, false);
    }

    /* ----------------------- cards ----------------------- */

    fn draw_cpu_card(&self, f: &mut Frame, area: Rect, theme: Theme) {
        if area.width < 24 || area.height < 4 { return; }
        let total_pct = self.last.map(|s| s.cpu_pct).unwrap_or(0.0);
        let ncpu = self.last_ncpu.max(1);
        let n = ncpu as f64;
        // normalize per core for the chart
        let hist_cpu_norm = map_hist(&self.hist_cpu, |v| v / n);
        let now_per_core = hist_cpu_norm.back().map(|&(_,v)| v).unwrap_or(0.0);
        let fmt_pct = |v: f64| if v.abs() < 1.0 { format!("{:.2}%", v) } else { format!("{:.1}%", v) };

        let title = Line::from(vec![
            Span::styled("   CPU  ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} /core  ", fmt_pct(now_per_core))),
            Span::styled(format!("(total {})  ", fmt_pct(total_pct)), Style::default().fg(theme.muted)),
            Span::styled(format!("({} cores)", ncpu), Style::default().fg(theme.muted)),
        ]);
        let card = Block::default().borders(Borders::ALL).title(title);
        f.render_widget(card.clone(), area);

        let inner = pad(area, 1);
        let [header_area, graph_area] = vstack(inner, [Constraint::Length(1), Constraint::Min(1)]);

        // CPU: absolute scale with local smart zoom (0 → min(adaptive, 1.25 * max_window))
        let (_mn_win, mx_win) = Self::range_last(&hist_cpu_norm, graph_area.width as usize);
        let local_cap = (mx_win * 1.25).max(0.02); // at least 0.02% so we see thin lines
        let hard_cap  = adaptive_percent_max(&hist_cpu_norm);
        let max_cpu   = hard_cap.min(local_cap);
        let scale_lbl = if max_cpu < 1.0 { format!("(scale 0–{:.2}%)", max_cpu) }
                        else            { format!("(scale 0–{:.0}%)", max_cpu) };

        let chips = chips_now_avg_peak(&hist_cpu_norm, "%", None, theme, Some(scale_lbl));
        f.render_widget(Paragraph::new(chips), header_area);

        if graph_area.height > 2 {
            let lines = multiline_bar_chart(
                &hist_cpu_norm,
                max_cpu,
                graph_area.width,
                graph_area.height,
                theme.muted,
                theme.accent,
            );
            f.render_widget(Paragraph::new(lines), graph_area);
        } else {
            let line = neo_sparkline_line(
                &hist_cpu_norm,
                max_cpu,
                graph_area.width,
                theme.muted,
                theme.accent,
            );
            f.render_widget(Paragraph::new(line), graph_area);
        }
    }

    fn draw_mem_card(&self, f: &mut Frame, area: Rect, theme: Theme) {
        if area.width < 24 || area.height < 4 { return; }
        let (used_mib, lim_mib, _ratio) = match self.last {
            Some(s) => {
                let used = s.mem_used_b / (1024.0 * 1024.0);
                let lim = s.mem_limit_b / (1024.0 * 1024.0);
                let r = if lim > 0.0 { (used / lim).clamp(0.0, 1.0) } else { 0.0 };
                (used, lim, r)
            }
            None => (0.0, 0.0, 0.0),
        };

        let title = if lim_mib > 0.0 {
            Line::from(vec![
                Span::styled("   MEM  ", Style::default().fg(theme.ok).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{:.0} / {:.0} MiB", used_mib, lim_mib)),
            ])
        } else {
            Line::from(vec![
                Span::styled("   MEM  ", Style::default().fg(theme.ok).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{:.0} MiB", used_mib)),
            ])
        };
        let card = Block::default().borders(Borders::ALL).title(title);
        f.render_widget(card.clone(), area);

        let inner = pad(area, 1);
        let [header_area, graph_area] = vstack(inner, [Constraint::Length(1), Constraint::Min(1)]);

        // Memory: absolute p95. If the curve is extremely flat, zoom the [min..max] range
        let p95_mib = percentile95(&self.hist_mem_mib).max(1.0);
        let (mn, mx) = Self::range_last(&self.hist_mem_mib, graph_area.width as usize);
        let span = (mx - mn).max(0.0);
        // ultra-flat = ~8% of the window or less, but at least 0.5 MiB span
        let ultra_flat = span / p95_mib <= 0.08 && span > 0.5;

        if ultra_flat {
            let hist_zoom = map_hist(&self.hist_mem_mib, |v| v - mn);
            let max_zoom = span.max(1.0);
            let lbl = format!("(zoom {:.0}–{:.0} MiB, Δ{:.0})", mn, mx, span);

            let chips = chips_now_avg_peak(&self.hist_mem_mib, "MiB", None, theme, Some(lbl));
            f.render_widget(Paragraph::new(chips), header_area);

            if graph_area.height > 2 {
                let lines = multiline_bar_chart(
                    &hist_zoom,
                    max_zoom,
                    graph_area.width,
                    graph_area.height,
                    theme.muted,
                    theme.ok,
                );
                f.render_widget(Paragraph::new(lines), graph_area);
            } else {
                let line = neo_sparkline_line(
                    &hist_zoom,
                    max_zoom,
                    graph_area.width,
                    theme.muted,
                    theme.ok,
                );
                f.render_widget(Paragraph::new(line), graph_area);
            }
        } else {
            let chips = chips_now_avg_peak(&self.hist_mem_mib, "MiB", Some(p95_mib), theme, None);
            f.render_widget(Paragraph::new(chips), header_area);

            if graph_area.height > 2 {
                let lines = multiline_bar_chart(
                    &self.hist_mem_mib,
                    p95_mib,
                    graph_area.width,
                    graph_area.height,
                    theme.muted,
                    theme.ok,
                );
                f.render_widget(Paragraph::new(lines), graph_area);
            } else {
                let line = neo_sparkline_line(
                    &self.hist_mem_mib,
                    p95_mib,
                    graph_area.width,
                    theme.muted,
                    theme.ok,
                );
                f.render_widget(Paragraph::new(line), graph_area);
            }
        }
    }

    fn draw_net_card(&self, f: &mut Frame, area: Rect, theme: Theme, rx: bool) {
        if area.width < 24 || area.height < 4 { return; }

        let (hist, label, icon) = if rx {
            (&self.hist_rx_bps, "NET↓", "  ")
        } else {
            (&self.hist_tx_bps, "NET↑", "  ")
        };

        let cur = hist.back().map(|(_,v)| *v).unwrap_or(0.0);
        let title = Line::from(vec![
            Span::styled(format!("{icon} {label}  "), Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)),
            Span::raw(human_bps(cur)),
        ]);
        let card = Block::default().borders(Borders::ALL).title(title);
        f.render_widget(card.clone(), area);

        let inner = pad(area, 1);
        let [header_area, graph_area] = vstack(inner, [Constraint::Length(1), Constraint::Min(1)]);

        // Network: absolute KiB/s p95. If ultra-flat, zoom the [min..max] range
        let hist_kib = map_hist(hist, |x| x/1024.0);
        let p95_kib = percentile95(&hist_kib).max(1.0);
        let (mn, mx) = Self::range_last(&hist_kib, graph_area.width as usize);
        let span = (mx - mn).max(0.0);
        // a bit more tolerant than memory: 15% of the window
        let ultra_flat = span / p95_kib <= 0.15 && span > 0.2;

        if ultra_flat {
            let hist_zoom = map_hist(&hist_kib, |v| v - mn);
            let max_zoom = span.max(1.0);
            let lbl = format!("(zoom {:.1}–{:.1} KiB/s, Δ{:.1})", mn, mx, span);

            let chips = chips_now_avg_peak(&hist_kib, "KiB/s", None, theme, Some(lbl));
            f.render_widget(Paragraph::new(chips), header_area);

            if graph_area.height > 2 {
                let lines = multiline_bar_chart(
                    &hist_zoom,
                    max_zoom,
                    graph_area.width,
                    graph_area.height,
                    theme.muted,
                    theme.muted,
                );
                f.render_widget(Paragraph::new(lines), graph_area);
            } else {
                let line = neo_sparkline_line(
                    &hist_zoom,
                    max_zoom,
                    graph_area.width,
                    theme.muted,
                    theme.muted,
                );
                f.render_widget(Paragraph::new(line), graph_area);
            }
        } else {
            let chips = chips_now_avg_peak(&hist_kib, "KiB/s", Some(p95_kib), theme, None);
            f.render_widget(Paragraph::new(chips), header_area);

            if graph_area.height > 2 {
                let lines = multiline_bar_chart(
                    &hist_kib,
                    p95_kib,
                    graph_area.width,
                    graph_area.height,
                    theme.muted,
                    theme.muted,
                );
                f.render_widget(Paragraph::new(lines), graph_area);
            } else {
                let line = neo_sparkline_line(
                    &hist_kib,
                    p95_kib,
                    graph_area.width,
                    theme.muted,
                    theme.muted,
                );
                f.render_widget(Paragraph::new(line), graph_area);
            }
        }
    }
}

/* ---------------- helpers: data & rendering ---------------- */

fn pointer_u64(v: &Value, ptr: &str) -> u64 { v.pointer(ptr).and_then(|x| x.as_u64()).unwrap_or(0) }

fn net_totals_bytes(v: &Value) -> (u64, u64) {
    if let Some(obj) = v.get("networks").and_then(|n| n.as_object()) {
        let mut rx = 0u64; let mut tx = 0u64;
        for ifc in obj.values() {
            rx = rx.saturating_add(ifc.get("rx_bytes").and_then(|x| x.as_u64()).unwrap_or(0));
            tx = tx.saturating_add(ifc.get("tx_bytes").and_then(|x| x.as_u64()).unwrap_or(0));
        }
        (rx, tx)
    } else { (0, 0) }
}

fn trim<T>(dq: &mut VecDeque<T>, cap: usize) {
    while dq.len() > cap { dq.pop_front(); }
}

/// Map helper to reuse the same utilities with a different numeric scale.
fn map_hist<F: Fn(f64)->f64>(h: &VecDeque<(Instant,f64)>, f: F) -> VecDeque<(Instant,f64)> {
    h.iter().map(|(t,y)| (*t, f(*y))).collect()
}

/// P95 (ignores spikes) to drive auto-scaling in a robust way.
fn percentile95(h: &VecDeque<(Instant, f64)>) -> f64 {
    if h.is_empty() { return 0.0; }
    let mut v: Vec<f64> = h.iter().map(|&(_,y)| y).collect();
    v.sort_by(|a,b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() as f64)*0.95).ceil() as usize - 1;
    v[idx.max(0).min(v.len()-1)]
}

fn adaptive_percent_max(h: &VecDeque<(Instant, f64)>) -> f64 {
    if h.is_empty() { return 100.0; }
    let p95 = percentile95(h);
    if p95 < 10.0 {
        // auto-zoom to reveal sub-1% loads, but keep a lower bound
        (p95 * 1.4).max(2.0)
    } else {
        100.0
    }
}

/// Build a line of Unicode block characters with a color gradient (absolute scale).
fn neo_sparkline_line(
    hist: &VecDeque<(Instant, f64)>,
    max: f64,
    width: u16,
    from: Color,
    to: Color,
) -> Line<'static> {
    let w = width as usize;
    if w == 0 { return Line::from(""); }

    // only the last w samples, right-aligned
    let take = hist.len().min(w);
    let start = hist.len().saturating_sub(take);

    let mut spans: Vec<Span> = Vec::with_capacity(w);

    // left padding if we have fewer samples than columns
    for _ in 0..(w - take) {
        spans.push(Span::styled(" ", Style::default()));
    }

    for &(_, y) in hist.iter().skip(start) {
        let v = if max > 0.0 { (y / max).clamp(0.0, 1.0) } else { 0.0 };
        let lvl = (v * 7.0).round() as usize;
        let ch = BLOCKS[lvl.min(7)];
        let c = color_lerp(from, to, v);
        spans.push(Span::styled(ch.to_string(), Style::default().fg(c)));
    }

    Line::from(spans)
}

/// "Now • Avg • Peak" chips + optional label (for scale / p95 / zoom info).
fn chips_now_avg_peak(
    hist: &VecDeque<(Instant, f64)>,
    unit: &str,
    max: Option<f64>,
    theme: Theme,
    label_override: Option<String>,
) -> Line<'static> {
    if hist.is_empty() { return Line::from(""); }
    let now = hist.back().unwrap().1;
    let avg = hist.iter().map(|&(_,y)| y).sum::<f64>() / (hist.len() as f64);
    let peak = hist.iter().map(|&(_,y)| y).fold(0.0, f64::max);
    let fmt = |v: f64| -> String {
        if unit == "%" {
            if v.abs() < 1.0 { format!("{:.2}{}", v, unit) } else { format!("{:.1}{}", v, unit) }
        } else if unit == "MiB" {
            format!("{:.0} {}", v, unit)
        } else {
            format!("{:.1} {}", v, unit)
        }
    };

    let mut parts = vec![
        Span::styled("  Now ", Style::default().fg(theme.muted)),
        Span::styled(fmt(now), Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
        Span::styled(" • Avg ", Style::default().fg(theme.muted)),
        Span::styled(fmt(avg), Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
        Span::styled(" • Peak ", Style::default().fg(theme.muted)),
        Span::styled(fmt(peak), Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
    ];

    if let Some(lbl) = label_override {
        parts.push(Span::raw("  "));
        parts.push(Span::styled(lbl, Style::default().fg(theme.muted)));
    } else if let Some(m) = max {
        parts.push(Span::raw("  "));
        let label = match unit {
            "%"      => format!("(scale 0–{:.0}%)", m),
            "MiB"    => format!("(p95 {:.0})", m),
            "KiB/s"  => format!("(p95 {:.0})", m),
            _        => format!("(scale {:.0})", m),
        };
        parts.push(Span::styled(label, Style::default().fg(theme.muted)));
    }
    Line::from(parts)
}

fn color_lerp(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = to_rgb(a);
    let (br, bg, bb) = to_rgb(b);
    Color::Rgb(
        (ar as f64 + (br as f64 - ar as f64) * t) as u8,
        (ag as f64 + (bg as f64 - ag as f64) * t) as u8,
        (ab as f64 + (bb as f64 - ab as f64) * t) as u8,
    )
}
fn to_rgb(c: Color) -> (u8,u8,u8) {
    match c {
        Color::Rgb(r,g,b) => (r,g,b),
        Color::Black => (0,0,0),
        Color::White => (255,255,255),
        Color::Gray => (128,128,128),
        Color::DarkGray => (64,64,64),
        Color::Red => (255,0,0),
        Color::Green => (0,255,0),
        Color::Blue => (0,0,255),
        Color::Yellow => (255,255,0),
        Color::Magenta => (255,0,255),
        Color::Cyan => (0,255,255),
        _ => (200,200,200),
    }
}

fn pad(r: Rect, pad: u16) -> Rect {
    Rect { x: r.x + pad, y: r.y + pad, width: r.width.saturating_sub(2*pad), height: r.height.saturating_sub(2*pad) }
}
fn vstack<const N: usize>(area: Rect, rows: [Constraint; N]) -> [Rect; N] {
    let rects = Layout::default().direction(Direction::Vertical).constraints(rows).split(area);
    std::array::from_fn(|i| rects[i])
}
fn human_bps(v: f64) -> String {
    let mut x = v.max(0.0);
    let units = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
    let mut i = 0;
    while x >= 1024.0 && i + 1 < units.len() { x /= 1024.0; i += 1; }
    format!("{:.1} {}", x, units[i])
}

/// Centered overlay (percentages 0–100)
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r)[1];
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert)[1]
}

/// Multi-line bar chart from [0..1] columns, using an absolute scale.
fn multiline_bar_chart(
    hist: &VecDeque<(Instant, f64)>,
    max: f64,
    width: u16,
    height: u16,
    from: Color,
    to: Color,
) -> Vec<Line<'static>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 { return vec![Line::from("")]; }

    // last w points, with left padding if needed
    let take = hist.len().min(w);
    let start = hist.len().saturating_sub(take);
    let mut cols = vec![0.0f64; w];
    for (i, &(_, y)) in hist.iter().skip(start).enumerate() {
        let v = if max > 0.0 { (y / max).clamp(0.0, 1.0) } else { 0.0 };
        cols[w - take + i] = v;
    }

    make_bar_lines(&cols, h, from, to)
}

/// Turn normalized column heights [0..1] into a set of text lines (top-down),
/// ensuring at least 1 row when v > 0 so that small values are still visible.
fn make_bar_lines(cols: &[f64], h: usize, from: Color, to: Color) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for row in (0..h).rev() {
        let mut spans = Vec::with_capacity(cols.len());
        for &v in cols {
            let filled_rows = if v <= 0.0 { 0 } else { (v * h as f64).ceil() as usize };
            let should_fill = row < filled_rows;
            if should_fill {
                let c = color_lerp(from, to, v);
                spans.push(Span::styled("█", Style::default().fg(c)));
            } else {
                spans.push(Span::raw(" "));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}
