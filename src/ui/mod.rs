use std::time::{Duration, Instant};

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Paragraph, Tabs},
};

use crate::{docker::DockerClient, theme::Theme};

pub mod containers;
pub use containers::ContainersView;

mod logs;
pub mod splash;
mod stats;
pub use logs::LogsPane;
pub use stats::StatsPane;

mod compose;
mod images;
mod networks;
mod volumes;

pub use compose::ComposeView;
pub use images::ImagesView;
pub use networks::NetworksView;
pub use volumes::VolumesView;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tab {
    Containers,
    Images,
    Networks,
    Compose,
    Volumes,
}

#[derive(Clone)]
pub struct Toast {
    pub msg: String,
    pub color: Color,
    pub until: Instant,
}

pub struct Toasts {
    list: Vec<Toast>,
}

impl Toasts {
    pub fn new() -> Self {
        Self { list: vec![] }
    }

    pub fn push(&mut self, msg: impl Into<String>, color: Color, ttl: Duration) {
        self.list.push(Toast {
            msg: msg.into(),
            color,
            until: Instant::now() + ttl,
        });
    }

    pub fn prune(&mut self) {
        self.list.retain(|t| Instant::now() < t.until);
    }

    pub fn render(&mut self) -> Option<(String, Color)> {
        self.prune();
        self.list.last().map(|t| (t.msg.clone(), t.color))
    }
}

pub struct Ui {
    pub theme: Theme,
    pub tab: Tab,
    pub containers: ContainersView,
    pub images: ImagesView,
    pub networks: NetworksView,
    pub compose: ComposeView,
    pub volumes: VolumesView,
    pub toasts: Toasts,
    pub logs: LogsPane,
    pub stats: StatsPane,
}

impl Ui {
    pub async fn new(docker: DockerClient) -> anyhow::Result<Self> {
        let theme = Theme::dark();
        Ok(Self {
            containers: ContainersView::new(docker.clone(), theme).await?,
            images: ImagesView::new(docker.clone(), theme).await?,
            networks: NetworksView::new(docker.clone(), theme).await?,
            compose: ComposeView::new(docker.clone(), theme).await?,
            volumes: VolumesView::new(docker.clone(), theme).await?,
            theme,
            tab: Tab::Containers,
            toasts: Toasts::new(),
            logs: LogsPane::new(docker.clone()),
            stats: StatsPane::new(docker),
        })
    }

    /// Return true if any modal / overlay is currently open.
    pub fn is_modal_open(&self) -> bool {
        self.logs.visible
            || self.stats.visible
            || self.containers.is_modal_open()
            || self.networks.is_modal_open()
            || self.compose.is_modal_open()
            || self.volumes.is_modal_open()
        // ImagesView currently has no modal, so it is not included here.
    }

    pub async fn on_tick(&mut self) -> anyhow::Result<()> {
        self.containers.on_tick();
        self.images.on_tick();
        self.networks.on_tick();
        self.compose.on_tick();
        self.volumes.on_tick();

        if self.stats.visible
            && let Some(id) = self.containers.selected_id()
        {
            self.stats.attach(&id);
        }

        self.logs.on_tick();
        self.stats.on_tick().await;

        if let Some((msg, col)) = self.containers.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }
        if let Some((msg, col)) = self.logs.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }
        if let Some((msg, col)) = self.images.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }
        if let Some((msg, col)) = self.networks.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }
        if let Some((msg, col)) = self.compose.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }
        if let Some((msg, col)) = self.volumes.last_note.take() {
            self.toasts.push(msg, col, Duration::from_secs(2));
        }

        Ok(())
    }

    pub async fn on_key(&mut self, key: crossterm::event::KeyEvent) -> anyhow::Result<()> {
        // 1) if a view has its own modal open, it gets priority
        if self.containers.has_modal() {
            self.containers.on_key(key).await?;
            return Ok(());
        }
        if self.networks.has_modal() {
            self.networks.on_key(key).await?;
            return Ok(());
        }
        if self.compose.has_modal() {
            self.compose.on_key(key).await?;
            return Ok(());
        }
        if self.volumes.has_modal() {
            self.volumes.on_key(key).await?;
            return Ok(());
        }

        // 2) logs and stats overlays take priority over tabs
        if self.logs.visible {
            match key.code {
                crossterm::event::KeyCode::Char('l') | crossterm::event::KeyCode::Esc => {
                    self.logs.toggle()
                }
                _ => self.logs.on_key(key),
            }
            return Ok(());
        }

        if self.stats.visible {
            match key.code {
                crossterm::event::KeyCode::Char('t') | crossterm::event::KeyCode::Esc => {
                    self.stats.set_visible(false)
                }
                _ => {}
            }
            return Ok(());
        }

        // 3) global shortcuts: logs / stats for selected container
        match key.code {
            crossterm::event::KeyCode::Char('l') => {
                if let Some(id) = self.containers.selected_id() {
                    self.logs.attach(&id);
                    self.logs.toggle();
                    self.logs.restart_follow();
                } else {
                    self.toasts.push(
                        "⚠ no container selected",
                        self.theme.warn,
                        Duration::from_secs(2),
                    );
                }
                return Ok(());
            }
            crossterm::event::KeyCode::Char('t') => {
                if let Some(id) = self.containers.selected_id() {
                    self.stats.attach(&id);
                    self.stats.set_visible(true);
                } else {
                    self.toasts.push(
                        "⚠ no container selected",
                        self.theme.warn,
                        Duration::from_secs(2),
                    );
                }
                return Ok(());
            }
            _ => {}
        }

        // 4) global tab navigation
        match key.code {
            crossterm::event::KeyCode::Char('1') => {
                self.tab = Tab::Containers;
                return Ok(());
            }
            crossterm::event::KeyCode::Char('2') => {
                self.tab = Tab::Images;
                return Ok(());
            }
            crossterm::event::KeyCode::Char('3') => {
                self.tab = Tab::Networks;
                return Ok(());
            }
            crossterm::event::KeyCode::Char('4') => {
                self.tab = Tab::Compose;
                return Ok(());
            }
            crossterm::event::KeyCode::Char('5') => {
                self.tab = Tab::Volumes;
                return Ok(());
            }
            crossterm::event::KeyCode::Tab => {
                self.next_tab();
                return Ok(());
            }
            crossterm::event::KeyCode::BackTab => {
                self.prev_tab();
                return Ok(());
            }
            _ => {}
        }

        // 5) delegate to active tab view
        match self.tab {
            Tab::Containers => self.containers.on_key(key).await?,
            Tab::Images => self.images.on_key(key).await?,
            Tab::Networks => self.networks.on_key(key).await?,
            Tab::Compose => self.compose.on_key(key).await?,
            Tab::Volumes => self.volumes.on_key(key).await?,
        }
        Ok(())
    }

    pub fn next_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Containers => Tab::Images,
            Tab::Images => Tab::Networks,
            Tab::Networks => Tab::Compose,
            Tab::Compose => Tab::Volumes,
            Tab::Volumes => Tab::Containers,
        }
    }

    pub fn prev_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Containers => Tab::Volumes,
            Tab::Images => Tab::Containers,
            Tab::Networks => Tab::Images,
            Tab::Compose => Tab::Networks,
            Tab::Volumes => Tab::Compose,
        }
    }

    fn footer_help_for_tab(&self) -> String {
        match self.tab {
            Tab::Containers => String::from(
                "q: quit • esc: back/close popup • 1–5/Tab: switch tabs • j/k: move \
                 • a: all/running • r/F5: refresh • Enter/Space: start/stop \
                 • R: restart • l: logs • t: stats",
            ),
            Tab::Images => String::from(
                "q: quit • esc: back/close popup • 1–5/Tab: switch tabs • j/k: move \
                 • /: search • a: all/dangling • o/O: sort • r/F5: refresh \
                 • i: inspect • d: delete • S: save visible list",
            ),
            Tab::Networks => String::from(
                "q: quit • esc: back/close popup • 1–5/Tab: switch tabs • j/k: move \
                 • /: search • a: all/user-defined • o/O: sort • r/F5: refresh \
                 • i: inspect • d: delete",
            ),
            Tab::Compose => String::from(
                "q: quit • esc: back/close popup • 1–5/Tab: switch tabs • j/k: move \
                 • /: search • r/F5: rescan • u: up -d • d: down • s: ps • l: logs",
            ),
            Tab::Volumes => String::from(
                "q: quit • esc: back/close popup • 1–5/Tab: switch tabs • j/k: move \
                 • /: search • o/O: sort • r/F5: refresh \
                 • i: inspect • d: delete • p: prune unused",
            ),
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let footer_h = if self.toasts.render().is_some() { 3 } else { 2 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(footer_h),
            ])
            .split(area);

        let titles = [" Containers", " Images", " Networks", " Compose", " Volumes"];

        let selected_idx = match self.tab {
            Tab::Containers => 0,
            Tab::Images => 1,
            Tab::Networks => 2,
            Tab::Compose => 3,
            Tab::Volumes => 4,
        };

        let tabs = Tabs::new(titles.iter().map(|t| Span::raw(*t)).collect::<Vec<_>>())
            .block(self.theme.block("DockrTUI"))
            .style(Style::default().fg(self.theme.fg))
            .highlight_style(
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .select(selected_idx);

        f.render_widget(tabs, chunks[0]);

        match self.tab {
            Tab::Containers => self.containers.draw(f, chunks[1]),
            Tab::Images => self.images.draw(f, chunks[1]),
            Tab::Networks => self.networks.draw(f, chunks[1]),
            Tab::Compose => self.compose.draw(f, chunks[1]),
            Tab::Volumes => self.volumes.draw(f, chunks[1]),
        }

        // overlays
        self.logs.draw(f, chunks[1], self.theme);
        self.stats.draw(f, chunks[1], self.theme);

        // footer
        let mut text = self.footer_help_for_tab();
        let mut style = Style::default().fg(self.theme.fg);

        if let Some((msg, col)) = self.toasts.render() {
            text = msg;
            style = Style::default()
                .fg(Color::Black)
                .bg(col)
                .add_modifier(Modifier::BOLD);
        }

        let footer_block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(self.theme.muted))
            .title(self.theme.title("Quick help"));

        let footer = Paragraph::new(text).style(style).block(footer_block);
        f.render_widget(footer, chunks[2]);
    }
}
