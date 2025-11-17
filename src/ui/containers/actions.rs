use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::docker::DockerClient;

#[derive(Clone, Copy)]
pub enum ActionKind {
    Starting,
    Stopping,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum BarPhase {
    Init,
    PullingImage,
    StartingRuntime,
    WaitingRunning,
    WaitingHealthy,
    StoppingSignal,
    WaitingExit,
    Done,
    Error,
}

#[derive(Clone)]
pub struct BarUpdate {
    pub pct: f32,        // 0..1
    pub phase: BarPhase, // current phase
}

pub struct ActionAnim {
    pub kind: ActionKind,
    pub name: String,
    #[allow(dead_code)]
    pub started: Instant,

    // Rocket (driven by the bar)
    #[allow(dead_code)]
    pub rocket_duration: Duration,
    pub rocket_t: f32, // 0..1

    // Bar (realistic)
    pub bar_pct: f32,
    pub bar_phase: BarPhase,
    pub rx: mpsc::UnboundedReceiver<BarUpdate>,
    pub bar_target_pct: f32,
    pub last_bar_tick: Instant,

    // Docker result
    pub done_flag: Arc<AtomicBool>,
    pub result: Arc<Mutex<Option<anyhow::Result<()>>>>,
    pub done_at: Option<Instant>,
}

pub async fn launch_action(
    docker: DockerClient,
    kind: ActionKind,
    id: String,
    name: String,
) -> Result<ActionAnim> {
    let (tx, rx) = mpsc::unbounded_channel::<BarUpdate>();

    let done_flag = Arc::new(AtomicBool::new(false));
    let result = Arc::new(Mutex::new(None));
    let done_c = done_flag.clone();
    let res_c = result.clone();

    // Docker task that drives the bar
    tokio::spawn(async move {
        let _ = tx.send(BarUpdate {
            pct: 0.05,
            phase: match kind {
                ActionKind::Starting => BarPhase::StartingRuntime,
                ActionKind::Stopping => BarPhase::StoppingSignal,
            },
        });

        let exec_res: anyhow::Result<()> = async {
            match kind {
                ActionKind::Starting => {
                    docker.start(&id).await.map(|_| ())?;
                    let _ = tx.send(BarUpdate { pct: 0.30, phase: BarPhase::StartingRuntime });

                    // wait for "running"
                    loop {
                        if let Ok(ins) = docker.inspect(&id).await {
                            let running = ins.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                            if running {
                                let _ = tx.send(BarUpdate { pct: 0.60, phase: BarPhase::WaitingRunning });

                                // healthcheck?
                                let has_health = ins.state.as_ref().and_then(|s| s.health.as_ref()).is_some();
                                if has_health {
                                    // wait for healthy
                                    loop {
                                        if let Ok(ins2) = docker.inspect(&id).await
                                            && let Some(h) = ins2.state.as_ref().and_then(|s| s.health.as_ref()) {
                                                if matches!(h.status, Some(ref s) if s.to_string().to_lowercase() == "healthy") {
                                                    let _ = tx.send(BarUpdate { pct: 0.98, phase: BarPhase::WaitingHealthy });
                                                    break;
                                                } else {
                                                    let _ = tx.send(BarUpdate { pct: 0.90, phase: BarPhase::WaitingHealthy });
                                                }
                                            }
                                        tokio::time::sleep(Duration::from_millis(240)).await;
                                    }
                                } else {
                                    let _ = tx.send(BarUpdate { pct: 0.92, phase: BarPhase::WaitingRunning });
                                }
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Ok(())
                }
                ActionKind::Stopping => {
                    let _ = tx.send(BarUpdate { pct: 0.25, phase: BarPhase::StoppingSignal });
                    docker.stop(&id, 2).await.map(|_| ())?;

                    let _ = tx.send(BarUpdate { pct: 0.60, phase: BarPhase::WaitingExit });
                    loop {
                        if let Ok(ins) = docker.inspect(&id).await {
                            let running = ins.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                            if !running {
                                let _ = tx.send(BarUpdate { pct: 0.98, phase: BarPhase::WaitingExit });
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(180)).await;
                    }
                    Ok(())
                }
            }
        }
        .await;

        *res_c.lock().unwrap() = Some(exec_res);
        done_c.store(true, Ordering::Relaxed);
        let _ = tx.send(BarUpdate {
            pct: 1.0,
            phase: BarPhase::Done,
        });
    });

    // Overlay (rocket follows the bar)
    let rocket_duration = match kind {
        ActionKind::Starting => Duration::from_millis(10_000),
        ActionKind::Stopping => Duration::from_millis(6_000),
    };

    Ok(ActionAnim {
        kind,
        name,
        started: Instant::now(),
        rocket_duration,
        rocket_t: 0.0,
        bar_pct: 0.0,
        bar_phase: BarPhase::Init,
        rx,
        bar_target_pct: 0.0,
        last_bar_tick: Instant::now(),
        done_flag,
        result,
        done_at: None,
    })
}
