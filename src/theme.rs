use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    #[allow(dead_code)]
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub accent_alt: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub muted: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: Color::Rgb(10, 12, 14),
            fg: Color::Rgb(224, 226, 228),
            accent: Color::Rgb(102, 166, 255),
            accent_alt: Color::Rgb(255, 149, 128),
            ok: Color::Rgb(46, 204, 113),
            warn: Color::Rgb(241, 196, 15),
            err: Color::Rgb(231, 76, 60),
            muted: Color::Rgb(120, 124, 130),
        }
    }

    pub fn title<'a>(&self, txt: &'a str) -> ratatui::widgets::block::Title<'a> {
        ratatui::widgets::block::Title::from(
            ratatui::text::Span::styled(txt, Style::default().fg(self.accent).add_modifier(Modifier::BOLD))
        )
    }

    pub fn block<'a>(&self, title: &'a str) -> ratatui::widgets::Block<'a> {
        ratatui::widgets::Block::default()
            .title(self.title(title))
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(Style::default().fg(self.muted))
    }

}
