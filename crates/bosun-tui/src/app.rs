//! The view's state and what a key does to it.
//!
//! [`App`] holds what the daemon last answered for each identity, where the cursor is, and the
//! question a pending action waits on. Nothing else: closing the TUI changes nothing (#4).
//! [`App::on_key`] turns a key into an [`Effect`]; only the run loop acts on one, so a test
//! can press keys and see exactly what would be sent.
//!
//! The operator actions come from the shared table ([`ACTIONS`]): a key is an action when a
//! row of the table names it, and the question asked is that row's confirmation. An action is
//! sent only after the question is answered `y`; any other key answers no and sends nothing.

use bosun_api::{CardView, Overview, Pace, QueueView, RepoOverview, RepoQueue, Resources};
use bosun_client::actions::{ACTIONS, Action, Scope, Spec, Target};

use crate::session::SessionView;

/// The tabs of this stage, in the order of the tab bar (#4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Queue,
    Session,
    Tokens,
    Resources,
}

impl Tab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Overview,
        Self::Queue,
        Self::Session,
        Self::Tokens,
        Self::Resources,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Queue => "Queue",
            Self::Session => "Session",
            Self::Tokens => "Tokens",
            Self::Resources => "Resources",
        }
    }

    /// The tab after this one, or before it, in the order of the tab bar.
    fn next(self, back: bool) -> Self {
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let n = Self::ALL.len();
        Self::ALL[if back { (at + n - 1) % n } else { (at + 1) % n }]
    }
}

/// A key, as the view reads it; the run loop maps the terminal's keys to these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Esc,
}

/// What the run loop is asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Perform an operator action: the question was asked and answered yes.
    Perform { action: Action, target: Target },
    /// Leave the TUI.
    Quit,
}

/// An action waiting on its question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) spec: &'static Spec,
    pub(crate) target: Target,
    /// The question, as the table words it for this target.
    pub(crate) question: String,
}

/// Whether the daemon answered the last refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Link {
    /// Not asked yet.
    Connecting,
    /// The last refresh was answered.
    Up,
    /// The last refresh was not: the view shows what it had, and the run loop reconnects.
    Down(String),
}

/// What one identity looked like at the last refresh.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct IdentityView {
    pub(crate) overview: Option<Overview>,
    pub(crate) queue: Option<QueueView>,
    /// The local clock when `overview` was answered, so the time in state keeps counting
    /// between refreshes.
    pub(crate) fetched_ms: i64,
    /// Why the identity's last refresh failed, when the daemon answered with an error.
    pub(crate) error: Option<String>,
    /// The last answer of `tokens.pace`, for the Tokens tab.
    pub(crate) pace: Option<Pace>,
    /// Why the last `tokens.pace` failed.
    pub(crate) pace_error: Option<String>,
    /// The last answer of `lease.list`, for the Resources tab.
    pub(crate) resources: Option<Resources>,
    /// Why the last `lease.list` failed.
    pub(crate) resources_error: Option<String>,
}

/// The whole view.
#[derive(Debug)]
pub(crate) struct App {
    /// The identities shown, each in a section of its own; never summed (#2).
    pub(crate) identities: Vec<(String, IdentityView)>,
    pub(crate) tab: Tab,
    /// The selected row of the Overview: an index into [`App::overview_rows`].
    pub(crate) overview_row: usize,
    /// The board the Queue shows: an index into [`App::boards`].
    pub(crate) board: usize,
    /// The selected card of that board: an index into [`App::cards`].
    pub(crate) card: usize,
    pub(crate) pending: Option<Pending>,
    /// The last thing worth saying: an action's answer, a refusal, a cancelled question.
    pub(crate) message: Option<String>,
    pub(crate) link: Link,
    /// The daemon's version, from the last overview.
    pub(crate) daemon: Option<String>,
    /// The live tail the Session tab shows: of [`App::followed`], or of the last item followed.
    pub(crate) session: Option<SessionView>,
}

impl App {
    pub(crate) fn new(identities: impl IntoIterator<Item = String>) -> Self {
        Self {
            identities: identities
                .into_iter()
                .map(|name| (name, IdentityView::default()))
                .collect(),
            tab: Tab::Overview,
            overview_row: 0,
            board: 0,
            card: 0,
            pending: None,
            message: None,
            link: Link::Connecting,
            daemon: None,
            session: None,
        }
    }

    /// The item the Session tab follows: the item in flight of the Overview's selected
    /// repository, else the first item in flight. `(identity, item)`.
    pub(crate) fn followed(&self) -> Option<(String, String)> {
        let rows = self.overview_rows();
        let in_flight = |(identity, _, repo): &(&str, &Overview, &RepoOverview)| {
            repo.current
                .as_ref()
                .map(|c| ((*identity).to_owned(), c.item.clone()))
        };
        rows.get(self.overview_row)
            .and_then(in_flight)
            .or_else(|| rows.iter().find_map(in_flight))
    }

    /// Every repository of the Overview, identity by identity: `(identity, repo)`.
    pub(crate) fn overview_rows(&self) -> Vec<(&str, &Overview, &RepoOverview)> {
        self.identities
            .iter()
            .filter_map(|(name, view)| view.overview.as_ref().map(|o| (name.as_str(), o)))
            .flat_map(|(name, o)| o.repos.iter().map(move |r| (name, o, r)))
            .collect()
    }

    /// Every board of the Queue, identity by identity.
    pub(crate) fn boards(&self) -> Vec<(&str, &RepoQueue)> {
        self.identities
            .iter()
            .filter_map(|(name, view)| view.queue.as_ref().map(|q| (name.as_str(), q)))
            .flat_map(|(name, q)| q.repos.iter().map(move |r| (name, r)))
            .collect()
    }

    /// The cards of `board` in board order: `(column name, card)`.
    pub(crate) fn cards(board: &RepoQueue) -> Vec<(&str, &CardView)> {
        board
            .columns
            .iter()
            .flat_map(|c| c.cards.iter().map(move |card| (c.column.as_str(), card)))
            .collect()
    }

    /// Keeps the cursors inside what there is after a refresh.
    pub(crate) fn clamp(&mut self) {
        let rows = self.overview_rows().len();
        self.overview_row = self.overview_row.min(rows.saturating_sub(1));
        let boards = self.boards();
        let board = self.board.min(boards.len().saturating_sub(1));
        let cards = boards.get(board).map_or(0, |(_, b)| Self::cards(b).len());
        self.board = board;
        self.card = self.card.min(cards.saturating_sub(1));
    }

    /// What a key does.
    pub(crate) fn on_key(&mut self, key: Key) -> Option<Effect> {
        if let Some(pending) = self.pending.take() {
            return if key == Key::Char('y') {
                self.message = Some(format!("{}: sent", pending.spec.label));
                Some(Effect::Perform {
                    action: pending.spec.action,
                    target: pending.target,
                })
            } else {
                self.message = Some(format!("{}: cancelled, nothing sent", pending.spec.label));
                None
            };
        }
        match key {
            Key::Char('q') | Key::Esc => return Some(Effect::Quit),
            Key::Char('1') => self.tab = Tab::Overview,
            Key::Char('2') => self.tab = Tab::Queue,
            Key::Char('3') => self.tab = Tab::Session,
            Key::Char('4') => self.tab = Tab::Tokens,
            Key::Char('5') => self.tab = Tab::Resources,
            Key::Tab => self.tab = self.tab.next(false),
            Key::BackTab => self.tab = self.tab.next(true),
            // `k` is the release of the kill switch, so up is the arrow only.
            Key::Up => self.step(false),
            Key::Down | Key::Char('j') => self.step(true),
            Key::Left | Key::Char('h') if self.tab == Tab::Queue => {
                self.board = self.board.saturating_sub(1);
                self.card = 0;
            }
            Key::Right | Key::Char('l') if self.tab == Tab::Queue => {
                self.board += 1;
                self.card = 0;
            }
            Key::Char(c) => {
                if let Some(spec) = ACTIONS.iter().find(|s| s.key == c) {
                    return self.ask(spec);
                }
            }
            _ => {}
        }
        self.clamp();
        None
    }

    fn step(&mut self, down: bool) {
        let cursor = match self.tab {
            Tab::Overview => &mut self.overview_row,
            Tab::Queue => &mut self.card,
            // The tail follows its end; the Tokens and Resources tabs have nothing to select:
            // no key acts on their rows.
            Tab::Session | Tab::Tokens | Tab::Resources => return,
        };
        *cursor = if down {
            *cursor + 1
        } else {
            cursor.saturating_sub(1)
        };
    }

    /// Asks the question of `spec` about what is selected; the mutant `mutant_tui_no_confirm`
    /// skips the question and sends at once.
    fn ask(&mut self, spec: &'static Spec) -> Option<Effect> {
        let (target, what) = match self.target(spec) {
            Ok(found) => found,
            Err(why) => {
                self.message = Some(format!("{}: {why}", spec.label));
                return None;
            }
        };
        #[cfg(mutant_tui_no_confirm)]
        {
            let _ = what;
            return Some(Effect::Perform {
                action: spec.action,
                target,
            });
        }
        #[cfg(not(mutant_tui_no_confirm))]
        {
            self.pending = Some(Pending {
                spec,
                question: spec.confirmation(&what),
                target,
            });
            None
        }
    }

    /// What `spec` acts on, from the selection, and how the question names it.
    fn target(&self, spec: &Spec) -> Result<(Target, String), String> {
        match spec.scope {
            Scope::Daemon => {
                if spec.action == Action::Kill && self.killed() {
                    let release = Action::ReleaseKill.spec();
                    return Err(format!(
                        "the kill switch is already thrown; `{}` (or `bosun {}`) releases it",
                        release.key, release.command
                    ));
                }
                Ok((Target::default(), "the daemon".to_owned()))
            }
            Scope::Repository => {
                let (identity, repo) = self.selected_repo().ok_or("no repository is selected")?;
                Ok((
                    Target {
                        identity: identity.to_owned(),
                        repo: Some(repo.to_owned()),
                        ..Target::default()
                    },
                    format!("{repo} ({identity})"),
                ))
            }
            Scope::Item => {
                let (identity, item) = self.selected_item().ok_or("no item is selected")?;
                Ok((
                    Target {
                        identity: identity.to_owned(),
                        item: Some(item.to_owned()),
                        ..Target::default()
                    },
                    item.to_owned(),
                ))
            }
        }
    }

    /// The kill switch is thrown, as the last overview said.
    pub(crate) fn killed(&self) -> bool {
        self.identities
            .iter()
            .any(|(_, v)| v.overview.as_ref().is_some_and(|o| o.killed))
    }

    /// The selected repository: the Overview's row (also under the Session and Tokens tabs),
    /// or the Queue's board.
    fn selected_repo(&self) -> Option<(&str, &str)> {
        match self.tab {
            Tab::Overview | Tab::Session | Tab::Tokens | Tab::Resources => self
                .overview_rows()
                .get(self.overview_row)
                .map(|(identity, _, repo)| (*identity, repo.repo.as_str())),
            Tab::Queue => self
                .boards()
                .get(self.board)
                .map(|(identity, board)| (*identity, board.repo.as_str())),
        }
    }

    /// The selected item: the Overview row's item in flight, the Queue's card, or the item
    /// the Session tab follows.
    fn selected_item(&self) -> Option<(&str, &str)> {
        match self.tab {
            Tab::Session => self
                .session
                .as_ref()
                .map(|s| (s.identity.as_str(), s.item.as_str())),
            Tab::Overview | Tab::Tokens | Tab::Resources => self
                .overview_rows()
                .get(self.overview_row)
                .and_then(|(identity, _, repo)| {
                    repo.current.as_ref().map(|c| (*identity, c.item.as_str()))
                }),
            Tab::Queue => {
                let boards = self.boards();
                let (identity, board) = boards.get(self.board)?;
                Self::cards(board)
                    .get(self.card)
                    .map(|(_, card)| (*identity, card.item.as_str()))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use bosun_api::{ColumnView, CurrentItem, LastVerdict, LintFinding, StreamStatus};

    use super::*;

    /// The keys the view itself takes; none of them may be an action's key.
    const VIEW_KEYS: &[char] = &['q', '1', '2', '3', '4', '5', 'j', 'h', 'l', 'y'];

    /// A synthetic identity with two repositories: one working on an item, one idle.
    pub(crate) fn overview(identity: &str, now_ms: i64) -> Overview {
        Overview {
            identity: identity.to_owned(),
            daemon_version: "0.0.0".to_owned(),
            started_ms: now_ms - 3_600_000,
            now_ms,
            killed: false,
            paused: false,
            daily_cap_until_ms: None,
            events: StreamStatus {
                epoch: "1-1".to_owned(),
                head: 41,
                subscribers: 1,
            },
            repos: vec![
                RepoOverview {
                    repo: "alpha".to_owned(),
                    path: "/work/alpha".to_owned(),
                    queue: "local_file".to_owned(),
                    queue_source: None,
                    state: "working".to_owned(),
                    hold: None,
                    tripped: None,
                    current: Some(CurrentItem {
                        item: "alpha#3".to_owned(),
                        state: "running".to_owned(),
                        detail: None,
                        attempt: 2,
                        since_ms: now_ms - 754_000,
                        waiting: Vec::new(),
                    }),
                    last_verdict: Some(LastVerdict {
                        item: "alpha#3".to_owned(),
                        attempt: 1,
                        run: 1,
                        result: "fail".to_owned(),
                        ended_ms: now_ms - 800_000,
                    }),
                },
                RepoOverview {
                    repo: "beta".to_owned(),
                    path: "/work/beta".to_owned(),
                    queue: "local_file".to_owned(),
                    queue_source: None,
                    state: "idle".to_owned(),
                    hold: Some("pause".to_owned()),
                    tripped: None,
                    current: None,
                    last_verdict: Some(LastVerdict {
                        item: "beta#7".to_owned(),
                        attempt: 1,
                        run: 2,
                        result: "pass".to_owned(),
                        ended_ms: now_ms - 9_000_000,
                    }),
                },
            ],
        }
    }

    fn card(repo: &str, n: u64, title: &str, state: Option<&str>) -> CardView {
        CardView {
            card: n,
            item: format!("{repo}#{n}"),
            title: title.to_owned(),
            labels: Vec::new(),
            draft: false,
            state: state.map(ToOwned::to_owned),
            snapshot_hash: None,
            edit_pending: false,
            lint: Vec::new(),
        }
    }

    /// The boards of [`overview`]'s repositories.
    pub(crate) fn queue(identity: &str) -> QueueView {
        let mut running = card("alpha", 3, "Parse the frame header", Some("running"));
        running.snapshot_hash = Some("9f2c4e1a7b3d5c6e".to_owned());
        running.edit_pending = true;
        let mut bad = card("alpha", 5, "Retry the upload", None);
        bad.lint = vec![
            LintFinding {
                severity: "error".to_owned(),
                message: "no contract block".to_owned(),
            },
            LintFinding {
                severity: "warning".to_owned(),
                message: "no labels".to_owned(),
            },
        ];
        let columns = |cards: [Vec<CardView>; 6]| {
            [
                "Backlog",
                "Ready",
                "In progress",
                "Verify",
                "Done",
                "Needs you",
            ]
            .into_iter()
            .zip(cards)
            .map(|(column, cards)| ColumnView {
                column: column.to_owned(),
                cards,
            })
            .collect()
        };
        QueueView {
            identity: identity.to_owned(),
            repos: vec![
                RepoQueue {
                    repo: "alpha".to_owned(),
                    queue: "local_file".to_owned(),
                    hash: Some("4be1d0c93a7f2e58".to_owned()),
                    error: None,
                    columns: columns([
                        vec![card("alpha", 9, "Someday: a second backend", None)],
                        vec![bad, card("alpha", 6, "Name the cache files", Some("ready"))],
                        vec![running],
                        Vec::new(),
                        vec![card("alpha", 1, "Read the config", Some("done"))],
                        Vec::new(),
                    ]),
                },
                RepoQueue {
                    repo: "beta".to_owned(),
                    queue: "local_file".to_owned(),
                    hash: None,
                    error: Some("the board file cannot be read".to_owned()),
                    columns: Vec::new(),
                },
            ],
        }
    }

    pub(crate) const NOW: i64 = 1_790_000_000_000;

    pub(crate) fn app() -> App {
        let mut app = App::new(["personal".to_owned()]);
        app.identities[0].1 = IdentityView {
            overview: Some(overview("personal", NOW)),
            queue: Some(queue("personal")),
            fetched_ms: NOW,
            error: None,
            pace: None,
            pace_error: None,
            resources: None,
            resources_error: None,
        };
        app.link = Link::Up;
        app.daemon = Some("0.0.0".to_owned());
        app
    }

    /// Every action of the shared table is reached by its key, asks its own question, and
    /// is sent only on `y`; no other key reaches an action.
    #[test]
    fn every_action_comes_from_the_shared_table() {
        for spec in ACTIONS {
            let mut app = app();
            if spec.scope == Scope::Item {
                // The selected row is `alpha`, which has an item in flight.
                assert_eq!(app.selected_item(), Some(("personal", "alpha#3")));
            }
            assert_eq!(app.on_key(Key::Char(spec.key)), None, "{spec:?}");
            let pending = app.pending.clone().expect("a question");
            assert_eq!(pending.spec, spec);
            let what = match spec.scope {
                Scope::Daemon => "the daemon",
                Scope::Repository => "alpha (personal)",
                Scope::Item => "alpha#3",
            };
            assert_eq!(pending.question, spec.confirmation(what));
            let Some(Effect::Perform { action, target }) = app.on_key(Key::Char('y')) else {
                panic!("{spec:?}: not sent on y");
            };
            assert_eq!(action, spec.action);
            match spec.scope {
                Scope::Daemon => assert_eq!(target, Target::default()),
                Scope::Repository => assert_eq!(target.repo.as_deref(), Some("alpha")),
                Scope::Item => assert_eq!(target.item.as_deref(), Some("alpha#3")),
            }
        }
        for key in VIEW_KEYS {
            assert!(
                ACTIONS.iter().all(|s| s.key != *key),
                "{key:?} is both a view key and an action's"
            );
        }
        // Every other printable key is no action.
        for c in (' '..='~').filter(|c| !VIEW_KEYS.contains(c)) {
            let mut app = app();
            let _ = app.on_key(Key::Char(c));
            assert_eq!(
                app.pending.is_some(),
                ACTIONS.iter().any(|s| s.key == c),
                "{c:?}"
            );
        }
    }

    #[test]
    fn any_answer_but_yes_sends_nothing() {
        for answer in [
            Key::Char('n'),
            Key::Esc,
            Key::Enter,
            Key::Char('Y'),
            Key::Char('p'),
        ] {
            let mut app = app();
            assert_eq!(app.on_key(Key::Char('p')), None);
            assert!(app.pending.is_some());
            assert_eq!(app.on_key(answer), None, "{answer:?}");
            assert!(app.pending.is_none());
            assert_eq!(
                app.message.as_deref(),
                Some("pause: cancelled, nothing sent")
            );
        }
    }

    #[test]
    fn restart_item_needs_an_item_and_the_queue_selects_one() {
        let mut app = app();
        let _ = app.on_key(Key::Down);
        assert_eq!(app.on_key(Key::Char('R')), None);
        assert!(app.pending.is_none(), "beta has no item in flight");
        assert_eq!(app.message.as_deref(), Some("restart: no item is selected"));

        let _ = app.on_key(Key::Char('2'));
        let _ = app.on_key(Key::Down);
        let _ = app.on_key(Key::Char('R'));
        assert_eq!(
            app.pending.as_ref().map(|p| p.target.item.as_deref()),
            Some(Some("alpha#5"))
        );
    }

    #[test]
    fn the_cursors_stay_on_what_there_is() {
        let mut app = app();
        for _ in 0..5 {
            let _ = app.on_key(Key::Down);
        }
        assert_eq!(app.overview_row, 1);
        let _ = app.on_key(Key::Char('2'));
        for _ in 0..5 {
            let _ = app.on_key(Key::Right);
        }
        assert_eq!(app.board, 1);
        assert_eq!(app.card, 0);
        assert_eq!(app.on_key(Key::Char('q')), Some(Effect::Quit));
    }

    #[test]
    fn the_kill_switch_is_not_thrown_twice() {
        let mut app = app();
        if let Some(o) = app.identities[0].1.overview.as_mut() {
            o.killed = true;
        }
        assert_eq!(app.on_key(Key::Char('K')), None);
        assert!(app.pending.is_none());
    }

    /// The kill switch is released from the TUI by its own row of the shared table: `k` asks
    /// the release question, `y` sends one `control.kill` with `release: true`, and any other
    /// answer sends nothing.
    #[tokio::test]
    async fn the_kill_switch_can_be_released_from_the_tui() {
        use bosun_api::Method as _;

        use crate::feed::Feed;
        use crate::feed::tests::{Fake, actions_of, token};

        let release = ACTIONS
            .iter()
            .find(|s| s.key == 'k')
            .expect("a row for `k`");
        assert_eq!(release.action, Action::ReleaseKill);
        assert_eq!(release.command, "kill --release");
        assert_eq!(release.method, bosun_api::method::ControlKill::NAME);
        assert_eq!(
            release.confirm,
            "Release the kill switch? Parked items resume and board writes start again."
        );

        let mut fake = Fake::new("release");
        let mut app = App::new(["personal".to_owned()]);
        let mut feed = Feed::new(fake.endpoint.clone()).with_operator(token);
        feed.refresh(&mut app, 0).await;
        assert_eq!(app.link, Link::Up);
        // Released or not, as the last overview said, the key asks.
        for killed in [true, false] {
            if let Some(o) = app.identities[0].1.overview.as_mut() {
                o.killed = killed;
            }
            for answer in [
                Key::Char('n'),
                Key::Char('k'),
                Key::Char('Y'),
                Key::Esc,
                Key::Enter,
            ] {
                assert!(!crate::on_key(&mut app, &mut feed, Key::Char('k')).await);
                assert_eq!(
                    app.pending.as_ref().map(|p| p.question.as_str()),
                    Some(release.confirmation("the daemon").as_str()),
                    "killed: {killed}"
                );
                assert!(!crate::on_key(&mut app, &mut feed, answer).await);
                assert!(app.pending.is_none(), "{answer:?}");
                assert_eq!(
                    app.message.as_deref(),
                    Some("release: cancelled, nothing sent")
                );
            }
        }
        assert_eq!(actions_of(&fake.calls()), Vec::<&str>::new());

        if let Some(o) = app.identities[0].1.overview.as_mut() {
            o.killed = true;
        }
        assert!(!crate::on_key(&mut app, &mut feed, Key::Char('k')).await);
        assert_eq!(actions_of(&fake.calls()), Vec::<&str>::new());
        assert!(!crate::on_key(&mut app, &mut feed, Key::Char('y')).await);
        assert_eq!(actions_of(&fake.calls()), vec!["control.kill"]);
        assert_eq!(fake.kills(), vec![true]);
        assert_eq!(app.message.as_deref(), Some("the kill switch is released"));
        fake.stop().await;
    }
}
