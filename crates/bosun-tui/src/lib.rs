//! The terminal client (#4, #73): a ratatui view over the local API, exposed as `bosun tui`.
//!
//! A client of the local API (#12): it depends on `bosun-client` and `bosun-api` only, and
//! holds no state beyond the view; closing it changes nothing (#4). [`run`] connects, takes
//! over the terminal and shows two tabs:
//!
//! | Tab | Content |
//! | --- | --- |
//! | Overview | per repository: state, current item, attempt, time in state, last verdict, hold; per identity its control state |
//! | Queue | one board at a time: its columns as lists, the board hash, each card's state, snapshot hash, edit-pending marker and lint findings |
//!
//! Identities are shown each in a section of its own, never summed (#2).
//!
//! # Keys
//!
//! `1`/`2` or Tab switch tabs, `j`/`k` (or the arrows) move, `h`/`l` switch boards in the
//! Queue, `q` or Esc quits. The operator actions (pause, resume, the kill switch,
//! restart-item) are the rows of the shared table `bosun_client::actions::ACTIONS`, with its
//! keys and its confirmation texts: each key asks its question first, and only `y` sends the
//! action; any other key answers no and nothing is sent. Pause and resume act on the selected
//! repository, restart-item on the selected item (the Overview row's item in flight, or the
//! Queue's card).
//!
//! # Refresh and reconnect
//!
//! One event subscription per identity asks for a refresh on every event or resync; the
//! refresh calls `status.overview` and `queue.get`. A daemon restart breaks the request
//! connection and is a `restarted` resync on the subscription: both reconnect by themselves,
//! and the view picks up the new daemon without being relaunched (`feed.rs`).

mod app;
mod feed;
mod view;

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bosun_client::Endpoint;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

use crate::app::{App, Effect, Key, Link};
use crate::feed::Feed;

/// What [`run`] needs.
#[derive(Debug, Clone)]
pub struct Options {
    /// The daemon's endpoint.
    pub endpoint: Endpoint,
    /// The identities to show, each in a section of its own.
    pub identities: Vec<String>,
}

/// Why [`run`] stopped short.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The daemon could not be reached at start, or speaks an unknown API version.
    Client(bosun_client::Error),
    /// The terminal could not be taken over or drawn on.
    Terminal(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(e) => e.fmt(f),
            Self::Terminal(e) => write!(f, "the terminal: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(e) => Some(e),
            Self::Terminal(e) => Some(e),
        }
    }
}

/// How often the view is redrawn with no input, so the time in state counts on and a daemon
/// that is away is asked again.
const TICK: Duration = Duration::from_secs(1);

/// Runs the TUI until the user quits.
///
/// # Errors
///
/// [`Error::Client`] when the daemon is not there at start (later it is waited for), or
/// [`Error::Terminal`].
pub async fn run(options: Options) -> Result<(), Error> {
    // Refuse a daemon that is down, or of an unknown version, before the terminal is taken.
    bosun_client::Client::connect(&options.endpoint)
        .await
        .map_err(Error::Client)?;
    let mut terminal = ratatui::try_init().map_err(Error::Terminal)?;
    let result = drive(&mut terminal, options).await;
    ratatui::restore();
    result
}

/// An input from the terminal.
#[derive(Debug)]
enum Input {
    Key(Key),
    /// The terminal changed size: draw again.
    Redraw,
}

async fn drive(terminal: &mut ratatui::DefaultTerminal, options: Options) -> Result<(), Error> {
    let mut app = App::new(options.identities.iter().cloned());
    let mut feed = Feed::new(options.endpoint.clone());
    let (tx, mut changes) = mpsc::unbounded_channel();
    let watchers: Vec<_> = options
        .identities
        .iter()
        .map(|name| feed::watch(options.endpoint.clone(), name.clone(), tx.clone()))
        .collect();
    drop(tx);
    let stop = Arc::new(AtomicBool::new(false));
    let mut inputs = read_terminal(Arc::clone(&stop));
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    feed.refresh(&mut app, now_ms()).await;
    let outcome = loop {
        if let Err(e) = terminal.draw(|f| view::draw(f, &app, now_ms())) {
            break Err(Error::Terminal(e));
        }
        tokio::select! {
            input = inputs.recv() => match input {
                Some(Input::Key(key)) => {
                    if on_key(&mut app, &mut feed, key).await {
                        break Ok(());
                    }
                }
                Some(Input::Redraw) => {}
                None => break Ok(()),
            },
            Some(_) = changes.recv() => {
                // One refresh answers every change signalled so far.
                while changes.try_recv().is_ok() {}
                feed.refresh(&mut app, now_ms()).await;
            }
            _ = tick.tick() => {
                if !matches!(app.link, Link::Up) {
                    feed.refresh(&mut app, now_ms()).await;
                }
            }
        }
    };
    stop.store(true, Ordering::Relaxed);
    for watcher in watchers {
        watcher.abort();
    }
    outcome
}

/// Handles one key: performs a confirmed action and refreshes. True when the user quits.
async fn on_key(app: &mut App, feed: &mut Feed, key: Key) -> bool {
    match app.on_key(key) {
        Some(Effect::Quit) => true,
        Some(Effect::Perform { action, target }) => {
            app.message = Some(feed.perform(action, &target).await);
            feed.refresh(app, now_ms()).await;
            false
        }
        None => false,
    }
}

/// Reads the terminal's input on a thread of its own until `stop`.
fn read_terminal(stop: Arc<AtomicBool>) -> mpsc::UnboundedReceiver<Input> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(false) => {}
                Ok(true) => {
                    let input = match event::read() {
                        Ok(Event::Key(key)) => key_of(key).map(Input::Key),
                        Ok(Event::Resize(..)) => Some(Input::Redraw),
                        Ok(_) => None,
                        Err(_) => return,
                    };
                    if let Some(input) = input
                        && tx.send(input).is_err()
                    {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    rx
}

/// The view's key for a terminal key; presses only (Windows also reports releases).
fn key_of(key: KeyEvent) -> Option<Key> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Char('q'),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyEventState;

    use super::*;

    #[test]
    fn only_presses_are_keys() {
        let press = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(key_of(press), Some(Key::Char('K')));
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
            ..press
        };
        assert_eq!(key_of(release), None);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_of(ctrl_c), Some(Key::Char('q')));
    }
}
