//! The interactive event loop: keys, trace updates and redraws.
//!
//! Replies only mark the screen dirty; a frame tick turns that into at most one redraw
//! per `FRAME`. Every frame renders a snapshot of the session, so the tracer never
//! waits for the terminal.

use super::events::{action_for, apply, Effect};
use super::render::render;
use super::state::UiState;
use crate::probe::Prober;
use crate::{tracer, MtrSession, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::{mpsc, Notify},
    time::MissedTickBehavior,
};
use tracing::debug;

/// Shortest time between two redraws caused by trace updates.
const FRAME: Duration = Duration::from_millis(50);

/// How often the input thread checks whether it should stop.
const INPUT_POLL: Duration = Duration::from_millis(100);

pub async fn run_interactive(mut session: MtrSession) -> Result<()> {
    // Open sockets before touching the terminal so failures print normally.
    let prober = session.open_probe_engine()?;

    // try_init installs a panic hook that restores the terminal before unwinding.
    let mut terminal = ratatui::try_init()?;
    let result = run_event_loop(&mut terminal, session, prober).await;
    ratatui::restore();
    result
}

/// Reads terminal events on its own thread, so no tokio worker blocks on stdin.
struct InputReader {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputReader {
    fn spawn(events: mpsc::UnboundedSender<Event>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match event::poll(INPUT_POLL) {
                    Ok(false) => {}
                    Ok(true) => {
                        let Ok(ev) = event::read() else { break };
                        if events.send(ev).is_err() {
                            break; // the event loop is gone
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for InputReader {
    /// Stop reading before the terminal is restored, so no keystroke meant for the
    /// shell is swallowed.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn draw(
    terminal: &mut DefaultTerminal,
    session: &Mutex<MtrSession>,
    ui: &mut UiState,
) -> Result<()> {
    let snapshot = session.lock().unwrap().clone();
    terminal.draw(|f| render(f, &snapshot, ui))?;
    Ok(())
}

async fn run_event_loop(
    terminal: &mut DefaultTerminal,
    mut session: MtrSession,
    prober: Box<dyn Prober>,
) -> Result<()> {
    let mut ui = UiState::new(&session.args);
    let updated = Arc::new(Notify::new());
    let notify = Arc::clone(&updated);
    session.set_update_callback(Arc::new(move || notify.notify_one()));
    let session = Arc::new(Mutex::new(session));

    let mut trace = tokio::spawn(tracer::run(Arc::clone(&session), prober));
    let mut tracing = true;

    let (tx, mut events) = mpsc::unbounded_channel();
    let _input = InputReader::spawn(tx);

    let mut frame = tokio::time::interval(FRAME);
    frame.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = false;
    draw(terminal, &session, &mut ui)?;

    let result = loop {
        tokio::select! {
            finished = &mut trace, if tracing => {
                tracing = false;
                match finished {
                    Ok(Err(e)) => break Err(e),
                    Err(e) if e.is_panic() => break Err(anyhow::anyhow!("trace task panicked: {e}")),
                    _ => debug!("Trace finished"),
                }
                ui.finished = true;
                dirty = true;
            }
            _ = updated.notified() => dirty = true,
            _ = frame.tick(), if dirty => {
                dirty = false;
                draw(terminal, &session, &mut ui)?;
            }
            event = events.recv() => {
                match event {
                    None => break Ok(()), // the input thread hit an error
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        // Raw mode swallows SIGINT, so Ctrl-C arrives as a key
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            break Ok(());
                        }
                        if let Some(action) = action_for(ui.mode, key) {
                            match apply(action, &mut ui) {
                                Effect::Quit => break Ok(()),
                                Effect::Reset => session.lock().unwrap().reset_statistics(),
                                Effect::None => {}
                            }
                        }
                    }
                    // Windows also reports key releases; resizes and the rest just redraw
                    Some(_) => {}
                }
                draw(terminal, &session, &mut ui)?;
            }
        }
    };

    trace.abort();
    result
}
