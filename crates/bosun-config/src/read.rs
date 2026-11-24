//! Reading the TOML document into a draft: the keys with their positions, the notices for what
//! a later stage reads, and the problems of shape (wrong type, missing key, refused value).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use bosun_core::{PathStyle, canonical_path};
use toml_edit::{Document, Item, TableLike};

use crate::error::{Location, Notice, Problem};
use crate::policy::{Policy, relative_entry};
use crate::{Budget, Daemon, Landing, LandingMode, McpServer, Queue, Verdict};

/// Sections of #4 that this version accepts and does not read.
const LATER_SECTIONS: &[&str] = &["notify"];

/// The default of `retention.reingest_window`: Claude Code's own default retention
/// (`cleanupPeriodDays`, 30 days), so `bosun doctor` is quiet on a default setup.
pub(crate) const DEFAULT_REINGEST_WINDOW: Duration = Duration::from_hours(30 * 24);

/// Units of `retention.reingest_window`: whole hours, days or weeks.
const WINDOW_UNITS: &[(char, u64)] = &[('h', 3600), ('d', 86_400), ('w', 604_800)];

/// Units of every other duration: seconds to weeks.
const DURATION_UNITS: &[(char, u64)] = &[
    ('s', 1),
    ('m', 60),
    ('h', 3600),
    ('d', 86_400),
    ('w', 604_800),
];

/// A path value: as written, its canonical key (#17) and where it is.
#[derive(Debug)]
pub(crate) struct PathValue {
    pub(crate) path: PathBuf,
    pub(crate) key: String,
    pub(crate) at: Option<Location>,
}

#[derive(Debug)]
pub(crate) struct DraftIdentity {
    pub(crate) name: String,
    pub(crate) claude_config_dir: Option<PathValue>,
    pub(crate) database: Option<PathValue>,
    pub(crate) github_host: String,
    pub(crate) github_login: Option<String>,
    pub(crate) gh_config_dir: Option<PathValue>,
    pub(crate) git_config: Option<PathValue>,
    pub(crate) ssh_key: Option<PathValue>,
    pub(crate) codex_home: Option<PathValue>,
    pub(crate) remote_allowlist: BTreeSet<String>,
    pub(crate) author_allowlist: BTreeSet<String>,
}

#[derive(Debug)]
pub(crate) struct DraftRepo {
    pub(crate) name: String,
    pub(crate) path: Option<PathValue>,
    pub(crate) identity: Option<(String, Option<Location>)>,
    pub(crate) queue: Option<Queue>,
    pub(crate) model: Option<String>,
    pub(crate) verdict: Verdict,
    pub(crate) policy: Policy,
    pub(crate) build_dirs: BTreeSet<String>,
    pub(crate) landing: Landing,
    pub(crate) mcp_servers: BTreeMap<String, McpServer>,
}

/// What the document says, in file order, before cross-checks.
#[derive(Debug)]
pub(crate) struct Draft {
    pub(crate) identities: Vec<DraftIdentity>,
    pub(crate) repos: Vec<DraftRepo>,
    pub(crate) reingest_window: Duration,
    pub(crate) budget: Budget,
    /// Where `budget.reserve_eqv` is written, for the cross-check against the daily cap.
    pub(crate) reserve_at: Option<Location>,
    pub(crate) daemon: Daemon,
    pub(crate) notices: Vec<Notice>,
}

struct Reader<'t> {
    text: &'t str,
    problems: Vec<Problem>,
    notices: Vec<Notice>,
}

/// Reads `text`. Problems are appended to `problems`; a malformed document yields `None`.
pub(crate) fn read(text: &str, problems: &mut Vec<Problem>) -> Option<Draft> {
    let document = match Document::parse(text) {
        Ok(document) => document,
        Err(e) => {
            problems.push(Problem {
                location: e.span().map(|span| Location::of(text, span.start)),
                message: e.message().trim().to_owned(),
            });
            return None;
        }
    };
    let mut reader = Reader {
        text,
        problems: Vec::new(),
        notices: Vec::new(),
    };
    let mut draft = Draft {
        identities: Vec::new(),
        repos: Vec::new(),
        reingest_window: DEFAULT_REINGEST_WINDOW,
        budget: Budget::default(),
        reserve_at: None,
        daemon: Daemon::default(),
        notices: Vec::new(),
    };

    let root = document.as_table();
    for (name, item) in root {
        let at = reader.key_at(root, name, item);
        match name {
            "daemon" => {
                if let Some(table) = reader.table(item, "daemon", at) {
                    reader.daemon(table, &mut draft.daemon);
                }
            }
            "identity" => {
                for (name, item, at) in reader.named_tables(item, "identity", at) {
                    draft.identities.push(reader.identity(name, item, at));
                }
            }
            "repo" => {
                for (name, item, at) in reader.named_tables(item, "repo", at) {
                    draft.repos.push(reader.repo(name, item, at));
                }
            }
            "retention" => {
                if let Some(window) = reader.retention(item, at) {
                    draft.reingest_window = window;
                }
            }
            "budget" => {
                if let Some(table) = reader.table(item, "budget", at) {
                    draft.reserve_at = reader.budget(table, &mut draft.budget);
                }
            }
            later if LATER_SECTIONS.contains(&later) => reader.notice(
                at,
                format!("section `{later}` is not read in this version; ignored"),
            ),
            unknown => reader.problem(
                at,
                format!(
                    "unknown section `{unknown}`; the sections are daemon, identity, repo, \
                     retention, budget and notify"
                ),
            ),
        }
    }

    problems.append(&mut reader.problems);
    draft.notices = reader.notices;
    Some(draft)
}

impl Reader<'_> {
    fn at(&self, span: Option<Range<usize>>) -> Option<Location> {
        span.map(|span| Location::of(self.text, span.start))
    }

    /// Where the key `name` of `table` is written, or its value when the key has no span.
    fn key_at(&self, table: &dyn TableLike, name: &str, item: &Item) -> Option<Location> {
        let span = table
            .get_key_value(name)
            .and_then(|(key, _)| key.span())
            .or_else(|| item.span());
        self.at(span)
    }

    fn problem(&mut self, location: Option<Location>, message: String) {
        self.problems.push(Problem { location, message });
    }

    fn notice(&mut self, location: Option<Location>, message: String) {
        self.notices.push(Notice { location, message });
    }

    fn ignored(&mut self, location: Option<Location>, path: &str) {
        self.notice(
            location,
            format!("`{path}` is not read in this version; ignored"),
        );
    }

    fn table<'d>(
        &mut self,
        item: &'d Item,
        what: &str,
        at: Option<Location>,
    ) -> Option<&'d dyn TableLike> {
        let table = item.as_table_like();
        if table.is_none() {
            self.problem(at, format!("`{what}` must be a table"));
        }
        table
    }

    /// The tables `<section>.<name>`, each with its name and position.
    fn named_tables<'d>(
        &mut self,
        item: &'d Item,
        section: &str,
        at: Option<Location>,
    ) -> Vec<(&'d str, &'d Item, Option<Location>)> {
        let Some(table) = self.table(item, section, at) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, item) in table.iter() {
            let at = self.key_at(table, name, item);
            let path = format!("{section}.{name}");
            if !valid_name(name) {
                self.problem(
                    at,
                    format!(
                        "`{path}`: a name is ASCII letters, digits, `-` and `_`, and is not empty"
                    ),
                );
            } else if item.as_table_like().is_none() {
                self.problem(at, format!("`{path}` must be a table"));
            } else {
                out.push((name, item, at));
            }
        }
        out
    }

    fn daemon(&mut self, table: &dyn TableLike, daemon: &mut Daemon) {
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("daemon.{key}");
            match key {
                "auto_pause" => set(&mut daemon.auto_pause, self.bool(item, &what)),
                "auto_pause_idle" => set(&mut daemon.auto_pause_idle, self.duration(item, &what)),
                "question_grace" => set(&mut daemon.question_grace, self.duration(item, &what)),
                "kill_grace" => set(&mut daemon.kill_grace, self.duration(item, &what)),
                "hello_timeout" => set(&mut daemon.hello_timeout, self.duration(item, &what)),
                "park_cap" => set(&mut daemon.park_cap, self.count(item, &what)),
                "mcp_call_cap" => set(&mut daemon.mcp_call_cap, self.count(item, &what)),
                _ => self.ignored(key_at, &what),
            }
        }
    }

    /// Reads `[budget]`; returns where `reserve_eqv` is written.
    fn budget(&mut self, table: &dyn TableLike, budget: &mut Budget) -> Option<Location> {
        let mut reserve_at = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("budget.{key}");
            match key {
                "item_eqv" => budget.item_eqv = self.positive(item, &what),
                "item_wall_clock" => budget.item_wall_clock = self.duration(item, &what),
                "daily_eqv" => budget.daily_eqv = self.positive(item, &what),
                "reserve_eqv" => {
                    reserve_at = self.at(item.span());
                    set(&mut budget.reserve_eqv, self.non_negative(item, &what));
                }
                _ => self.ignored(key_at, &what),
            }
        }
        reserve_at
    }

    fn identity(&mut self, name: &str, item: &Item, at: Option<Location>) -> DraftIdentity {
        let path = format!("identity.{name}");
        let mut identity = DraftIdentity {
            name: name.to_owned(),
            claude_config_dir: None,
            database: None,
            github_host: crate::DEFAULT_GITHUB_HOST.to_owned(),
            github_login: None,
            gh_config_dir: None,
            git_config: None,
            ssh_key: None,
            codex_home: None,
            remote_allowlist: BTreeSet::new(),
            author_allowlist: BTreeSet::new(),
        };
        let Some(table) = item.as_table_like() else {
            return identity;
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("{path}.{key}");
            match key {
                "claude_config_dir" => identity.claude_config_dir = self.path(item, &what),
                "database" => identity.database = self.path(item, &what),
                "github_host" => {
                    if let Some(host) = self.word(item, &what) {
                        identity.github_host = host;
                    }
                }
                "github_login" => identity.github_login = self.word(item, &what),
                "gh_config_dir" => identity.gh_config_dir = self.path(item, &what),
                "git_config" => identity.git_config = self.path(item, &what),
                "ssh_key" => identity.ssh_key = self.path(item, &what),
                "codex_home" => identity.codex_home = self.path(item, &what),
                "remote_allowlist" => {
                    identity.remote_allowlist = self.words(item, &what).unwrap_or_default();
                }
                "author_allowlist" => {
                    identity.author_allowlist = self.words(item, &what).unwrap_or_default();
                }
                _ => self.ignored(key_at, &what),
            }
        }
        for (key, value) in [
            ("claude_config_dir", identity.claude_config_dir.is_some()),
            ("database", identity.database.is_some()),
        ] {
            if !value && table.get(key).is_none() {
                self.problem(at, format!("`{path}` has no `{key}`"));
            }
        }
        identity
    }

    fn repo(&mut self, name: &str, item: &Item, at: Option<Location>) -> DraftRepo {
        let path = format!("repo.{name}");
        let mut repo = DraftRepo {
            name: name.to_owned(),
            path: None,
            identity: None,
            queue: None,
            model: None,
            verdict: Verdict::default(),
            policy: Policy::default(),
            build_dirs: BTreeSet::new(),
            landing: Landing::default(),
            mcp_servers: BTreeMap::new(),
        };
        let Some(table) = item.as_table_like() else {
            return repo;
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("{path}.{key}");
            match key {
                "path" => repo.path = self.path(item, &what),
                "identity" => {
                    repo.identity = self
                        .string(item, &what)
                        .map(|(value, at)| (value.to_owned(), at));
                }
                "queue" => repo.queue = self.queue(item, &what, key_at),
                "model" => repo.model = self.word(item, &what),
                "verdict" => {
                    if let Some(table) = self.table(item, &what, key_at) {
                        self.verdict(table, &what, &mut repo.verdict);
                    }
                }
                "policy" => {
                    if let Some(table) = self.table(item, &what, key_at) {
                        repo.policy = self.policy(table, &what);
                    }
                }
                "build_dirs" => repo.build_dirs = self.entries(item, &what).unwrap_or_default(),
                "landing" => {
                    if let Some(table) = self.table(item, &what, key_at) {
                        self.landing(table, &what);
                    }
                }
                "mcp_servers" => repo.mcp_servers = self.mcp_servers(item, &what, key_at),
                _ => self.ignored(key_at, &what),
            }
        }
        for key in ["path", "identity"] {
            if table.get(key).is_none() {
                self.problem(at, format!("`{path}` has no `{key}`"));
            }
        }
        // Fail closed (#133): with no model of its own, a session runs on whatever the agent
        // CLI defaults to that day. An ingest-only repository spawns nothing and needs none.
        let unchecked = cfg!(mutant_config_queue_without_model);
        if !unchecked && table.get("queue").is_some() && table.get("model").is_none() {
            self.problem(
                at,
                format!(
                    "`{path}` has a `queue` and no `model`: the default model of its sessions,                      e.g. `model = \"opus\"`"
                ),
            );
        }
        repo
    }

    fn queue(&mut self, item: &Item, what: &str, at: Option<Location>) -> Option<Queue> {
        let table = self.table(item, what, at)?;
        let Some(kind_item) = table.get("kind") else {
            self.problem(
                at,
                format!("`{what}` has no `kind`; it is \"local_file\" or \"projects\""),
            );
            return None;
        };
        let (kind, kind_at) = self.string(kind_item, &format!("{what}.kind"))?;
        let mut file = None;
        let mut owner = None;
        let mut number = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match (kind, key) {
                (_, "kind") => {}
                ("local_file", "file") => file = self.path(item, &path),
                ("projects", "owner") => owner = self.word(item, &path),
                ("projects", "number") => number = self.positive(item, &path),
                _ => self.ignored(key_at, &path),
            }
        }
        let missing = |key: &str| format!("`{what}` of kind \"{kind}\" has no `{key}`");
        match kind {
            "local_file" => {
                if file.is_none() && table.get("file").is_none() {
                    self.problem(at, missing("file"));
                }
                Some(Queue::LocalFile { file: file?.path })
            }
            "projects" => {
                for (key, found) in [("owner", owner.is_some()), ("number", number.is_some())] {
                    if !found && table.get(key).is_none() {
                        self.problem(at, missing(key));
                    }
                }
                Some(Queue::Projects {
                    owner: owner?,
                    number: number?,
                })
            }
            other => {
                self.problem(
                    kind_at,
                    format!(
                        "`{what}.kind` is {other:?}; it is \"local_file\" or \"projects\" \
                         (a Projects v2 board)"
                    ),
                );
                None
            }
        }
    }

    fn verdict(&mut self, table: &dyn TableLike, what: &str, verdict: &mut Verdict) {
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "timeout" => set(&mut verdict.timeout, self.duration(item, &path)),
                "shell" => {
                    verdict.shell = self
                        .string(item, &path)
                        .map(|(value, _)| value.to_owned())
                        .filter(|shell| !shell.is_empty());
                }
                _ => self.ignored(key_at, &path),
            }
        }
    }

    /// Reads a policy. An unknown key is a problem, not a notice: a misspelt `protected` must
    /// not silently leave the paths unprotected.
    fn policy(&mut self, table: &dyn TableLike, what: &str) -> Policy {
        let mut protected = BTreeSet::new();
        let mut push = false;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "protected" => protected = self.entries(item, &path).unwrap_or_default(),
                "push" => set(&mut push, self.bool(item, &path)),
                other => self.problem(
                    key_at,
                    format!("unknown key `{what}.{other}`; a policy has `protected` and `push`"),
                ),
            }
        }
        Policy::new(protected, push)
    }

    /// Reads `landing`; stage 1 lands `local` only (#7), so every other mode is refused.
    fn landing(&mut self, table: &dyn TableLike, what: &str) {
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            if key != "mode" {
                self.ignored(key_at, &path);
                continue;
            }
            let Some((mode, at)) = self.string(item, &path) else {
                continue;
            };
            match LandingMode::parse(mode) {
                Some(LandingMode::Local) => {}
                None if matches!(mode, "merge" | "pr") => self.problem(
                    at,
                    format!(
                        "`{path}` is {mode:?}; this version lands \"local\" only: the \
                         \"merge\" and \"pr\" modes come with #8"
                    ),
                ),
                None => self.problem(
                    at,
                    format!(
                        "`{path}` is {mode:?}; it is \"local\" (\"merge\" and \"pr\" come \
                         with #8)"
                    ),
                ),
            }
        }
    }

    fn mcp_servers(
        &mut self,
        item: &Item,
        what: &str,
        at: Option<Location>,
    ) -> BTreeMap<String, McpServer> {
        let mut servers = BTreeMap::new();
        for (name, item, at) in self.named_tables(item, what, at) {
            let path = format!("{what}.{name}");
            if name == crate::RESERVED_MCP_SERVER {
                self.problem(
                    at,
                    format!("`{path}`: the name `bosun` is the daemon's own server"),
                );
                continue;
            }
            let Some(table) = item.as_table_like() else {
                continue;
            };
            let mut command = None;
            let mut server = McpServer {
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
            };
            for (key, item) in table.iter() {
                let key_at = self.key_at(table, key, item);
                let key_path = format!("{path}.{key}");
                match key {
                    "command" => command = self.word(item, &key_path),
                    "args" => server.args = self.strings(item, &key_path).unwrap_or_default(),
                    "env" => server.env = self.string_map(item, &key_path, key_at),
                    _ => self.ignored(key_at, &key_path),
                }
            }
            if let Some(command) = command {
                server.command = command;
                servers.insert(name.to_owned(), server);
            } else if table.get("command").is_none() {
                self.problem(at, format!("`{path}` has no `command`"));
            }
        }
        servers
    }

    fn retention(&mut self, item: &Item, at: Option<Location>) -> Option<Duration> {
        let table = self.table(item, "retention", at)?;
        let mut window = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            if key == "reingest_window" {
                let (text, at) = self.string(item, "retention.reingest_window")?;
                window = parse_duration(text, WINDOW_UNITS);
                if window.is_none() {
                    self.problem(
                        at,
                        format!(
                            "`retention.reingest_window` is {text:?}; expected a positive \
                             whole number of hours, days or weeks, such as \"30d\""
                        ),
                    );
                }
            } else {
                self.ignored(key_at, &format!("retention.{key}"));
            }
        }
        window
    }

    fn string<'d>(&mut self, item: &'d Item, what: &str) -> Option<(&'d str, Option<Location>)> {
        let at = self.at(item.span());
        let value = item.as_str();
        if value.is_none() {
            self.problem(at, format!("`{what}` must be a string"));
        }
        value.map(|value| (value, at))
    }

    /// A non-empty string with no surrounding whitespace: a login, a host, a command.
    fn word(&mut self, item: &Item, what: &str) -> Option<String> {
        let (text, at) = self.string(item, what)?;
        if text.is_empty() || text.trim() != text {
            self.problem(
                at,
                format!("`{what}` is {text:?}; it must be non-empty, with no surrounding spaces"),
            );
            return None;
        }
        Some(text.to_owned())
    }

    fn bool(&mut self, item: &Item, what: &str) -> Option<bool> {
        let value = item.as_bool();
        if value.is_none() {
            let at = self.at(item.span());
            self.problem(at, format!("`{what}` must be true or false"));
        }
        value
    }

    fn integer(&mut self, item: &Item, what: &str, min: i64) -> Option<u64> {
        let at = self.at(item.span());
        let value = item
            .as_integer()
            .filter(|&n| n >= min)
            .and_then(|n| u64::try_from(n).ok());
        if value.is_none() {
            let shape = if min > 0 { "positive" } else { "non-negative" };
            self.problem(at, format!("`{what}` must be a {shape} whole number"));
        }
        value
    }

    fn positive(&mut self, item: &Item, what: &str) -> Option<u64> {
        self.integer(item, what, 1)
    }

    fn non_negative(&mut self, item: &Item, what: &str) -> Option<u64> {
        self.integer(item, what, 0)
    }

    /// A positive count that fits 32 bits.
    fn count(&mut self, item: &Item, what: &str) -> Option<u32> {
        let value = self.positive(item, what)?;
        let count = u32::try_from(value).ok();
        if count.is_none() {
            let at = self.at(item.span());
            self.problem(at, format!("`{what}` is too large"));
        }
        count
    }

    fn duration(&mut self, item: &Item, what: &str) -> Option<Duration> {
        let (text, at) = self.string(item, what)?;
        let value = parse_duration(text, DURATION_UNITS);
        if value.is_none() {
            self.problem(
                at,
                format!(
                    "`{what}` is {text:?}; expected a positive whole number of seconds, minutes, \
                     hours, days or weeks, such as \"90s\", \"30m\" or \"2h\""
                ),
            );
        }
        value
    }

    /// An array of strings, each with its position.
    fn string_array<'d>(
        &mut self,
        item: &'d Item,
        what: &str,
    ) -> Option<Vec<(&'d str, Option<Location>)>> {
        let at = self.at(item.span());
        let Some(array) = item.as_array() else {
            self.problem(at, format!("`{what}` must be an array of strings"));
            return None;
        };
        let mut out = Vec::new();
        for value in array {
            let value_at = self.at(value.span());
            match value.as_str() {
                Some(text) => out.push((text, value_at)),
                None => self.problem(value_at, format!("`{what}` must hold strings only")),
            }
        }
        Some(out)
    }

    fn strings(&mut self, item: &Item, what: &str) -> Option<Vec<String>> {
        let array = self.string_array(item, what)?;
        Some(array.into_iter().map(|(s, _)| s.to_owned()).collect())
    }

    /// A set of words (logins, owners): order and duplicates do not matter.
    fn words(&mut self, item: &Item, what: &str) -> Option<BTreeSet<String>> {
        let array = self.string_array(item, what)?;
        let mut out = BTreeSet::new();
        for (text, at) in array {
            if text.is_empty() || text.trim() != text {
                self.problem(
                    at,
                    format!("`{what}` holds {text:?}; an entry is non-empty, with no spaces"),
                );
            } else {
                out.insert(text.to_owned());
            }
        }
        Some(out)
    }

    /// A set of repository-relative entries (protected paths, build directories).
    fn entries(&mut self, item: &Item, what: &str) -> Option<BTreeSet<String>> {
        let array = self.string_array(item, what)?;
        let mut out = BTreeSet::new();
        for (text, at) in array {
            match relative_entry(text) {
                Ok(entry) => {
                    out.insert(entry);
                }
                Err(why) => self.problem(at, format!("`{what}` holds {text:?}: {why}")),
            }
        }
        Some(out)
    }

    fn string_map(
        &mut self,
        item: &Item,
        what: &str,
        at: Option<Location>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let Some(table) = self.table(item, what, at) else {
            return out;
        };
        for (key, item) in table.iter() {
            let path = format!("{what}.{key}");
            if let Some((value, _)) = self.string(item, &path) {
                out.insert(key.to_owned(), value.to_owned());
            }
        }
        out
    }

    fn path(&mut self, item: &Item, what: &str) -> Option<PathValue> {
        let (text, at) = self.string(item, what)?;
        let path = PathBuf::from(text);
        if !path.is_absolute() {
            self.problem(
                at,
                format!("`{what}` is {text:?}; a path in the configuration must be absolute"),
            );
            return None;
        }
        Some(PathValue {
            key: canonical_path(text, PathStyle::host()),
            path,
            at,
        })
    }
}

/// Replaces `target` when a value was read; a refused value keeps the default.
fn set<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parses `<n><unit>` with `n > 0` and the unit one of `units` (its letter and its seconds).
fn parse_duration(text: &str, units: &[(char, u64)]) -> Option<Duration> {
    let unit = text.chars().last()?;
    let &(_, seconds) = units.iter().find(|(letter, _)| *letter == unit)?;
    let count = &text[..text.len() - unit.len_utf8()];
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let count: u64 = count.parse().ok().filter(|&n| n > 0)?;
    Some(Duration::from_secs(count.checked_mul(seconds)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reingest_windows() {
        let window = |text| parse_duration(text, WINDOW_UNITS);
        assert_eq!(window("30d"), Some(Duration::from_hours(30 * 24)));
        assert_eq!(window("12h"), Some(Duration::from_hours(12)));
        assert_eq!(window("2w"), Some(Duration::from_hours(14 * 24)));
        for bad in [
            "",
            "d",
            "0d",
            "-1d",
            "+1d",
            "1.5d",
            "30",
            "30m",
            "30 d",
            "99999999999999999999d",
        ] {
            assert_eq!(window(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn durations() {
        let duration = |text| parse_duration(text, DURATION_UNITS);
        assert_eq!(duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(duration("30m"), Some(Duration::from_mins(30)));
        assert_eq!(duration("2h"), Some(Duration::from_hours(2)));
        for bad in ["", "s", "0s", "1.5h", "30", "30y", "5 m", "й"] {
            assert_eq!(duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn names() {
        assert!(valid_name("work-2_x"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("рабочий"));
    }
}
