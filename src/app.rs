use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal, restore};
use tokio::sync::mpsc;

use crate::{docker::DockerClient, ui::Ui};
use crate::theme::Theme;

// Animated splash screen
use crate::ui::splash::{SplashScreen, SplashEvent};

pub const TERMINAL_MIN_WIDTH: u16 = 70;
pub const TERMINAL_MIN_HEIGHT: u16 = 28;

enum Route {
    Splash,
    Main,
}

pub async fn run() -> Result<()> {
    // --- terminal ---
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let term_size = terminal.size()?;
    if term_size.height < TERMINAL_MIN_HEIGHT || term_size.width < TERMINAL_MIN_WIDTH {
        println!(
            "Error: terminal size should be at least {}x{}, current is {}x{}",
            TERMINAL_MIN_WIDTH, TERMINAL_MIN_HEIGHT, term_size.width, term_size.height
        );
        std::process::exit(1);
    }

    enable_raw_mode()?;
    set_panic_hook();
    terminal.clear()?;

    // --- Splash + control channel ---
    // (uses Theme palette; adapt if needed)
    let (mut splash, splash_tx) = SplashScreen::with_channel(Theme::dark());

    // We'll receive the ready UI via this channel
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Result<Ui>>();

    // --- Asynchronous initialization task ---
    tokio::spawn(async move {
        let _ = splash_tx.send(SplashEvent::Step { pct: 0.10, label: "Connect to Docker…".into() });
        let docker = match DockerClient::connect_default().await {
            Ok(d) => d,
            Err(e) => {
                let _ = splash_tx.send(SplashEvent::Fail(format!("docker connect: {e}")));
                let _ = ui_tx.send(Err(e));
                return;
            }
        };

        let _ = splash_tx.send(SplashEvent::Step { pct: 0.45, label: "Warm-up containers…".into() });
        if let Err(e) = docker.list_containers(true).await {
            let _ = splash_tx.send(SplashEvent::Fail(format!("list: {e}")));
            let _ = ui_tx.send(Err(e));
            return;
        }

        let _ = splash_tx.send(SplashEvent::Step { pct: 0.70, label: "Build UI state…".into() });
        let ui = Ui::new(docker).await;

        match ui {
            Ok(ui) => {
                let _ = splash_tx.send(SplashEvent::Step { pct: 0.95, label: "Final touches…".into() });
                let _ = splash_tx.send(SplashEvent::Done);
                let _ = ui_tx.send(Ok(ui));
            }
            Err(e) => {
                let _ = splash_tx.send(SplashEvent::Fail(format!("ui: {e}")));
                let _ = ui_tx.send(Err(e));
            }
        }
    });

    // --- Route + state ---
    let mut route = Route::Splash;
    let mut ui: Option<Ui> = None;

    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(120);

    // --- event loop ---
    'outer: loop {
        terminal.draw(|f| match route {
            Route::Splash => splash.draw(f, f.area()),
            Route::Main => {
                if let Some(u) = ui.as_mut() {
                    u.draw(f, f.area());
                }
            }
        })?;

        // timeout for event polling
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::from_millis(0));

        // input
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match route {
                    Route::Splash => {
                        // 'q' = quit, 'Esc'/'Enter' = skip splash
                        if matches!(key.code, KeyCode::Char('q')) {
                            break 'outer;
                        }
                        splash.on_key(key);
                    }
                    Route::Main => {
                        if let Some(u) = ui.as_mut() {
                            // 1) let the UI handle the key (popups, shell, etc.)
                            u.on_key(key).await?;

                            // 2) Quit ONLY if 'q' and no modal is open
                            if matches!(key.code, KeyCode::Char('q')) && !u.is_modal_open() {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        // tick
        if last_tick.elapsed() >= tick_rate {
            match route {
                Route::Splash => {
                    splash.on_tick();

                    // Try to retrieve the ready UI (non-blocking)
                    if ui.is_none() {
                        if let Ok(res) = ui_rx.try_recv() {
                            match res {
                                Ok(u) => ui = Some(u),
                                Err(_e) => {
                                    // Failure is displayed in the splash; stay on splash
                                }
                            }
                        }
                    }

                    // If UI is ready AND splash is ready to close -> go Main
                    if ui.is_some() && splash.is_ready_to_close() {
                        route = Route::Main;
                    }
                }
                Route::Main => {
                    if let Some(u) = ui.as_mut() {
                        u.on_tick().await?;
                    }
                }
            }

            last_tick = Instant::now();
        }
    }

    // --- restore ---
    disable_raw_mode()?;
    let mut stdout: io::Stdout = std::io::stdout();
    execute!(stdout, LeaveAlternateScreen)?;
    Ok(())
}

fn set_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore();
        hook(panic_info);
    }));
}
