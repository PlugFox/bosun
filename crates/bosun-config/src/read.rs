//! Reading the TOML document into a draft: the keys with their positions, the notices for what
//! a later stage reads, and the problems of shape (wrong type, missing key, refused value).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use bosun_core::{PathStyle, Scope, canonical_path};
use toml_edit::{Document, Item, TableLike};

use crate::error::{Location, Notice, Problem};
use crate::policy::{Census, Policy, relative_entry};
use crate::{
    ARTIFACT_TIMEOUT, Artifact, Budget, Daemon, Landing, LandingMode, McpServer, PROBE_TIMEOUT,
    PROBE_TIMEOUT_MAX, PullRequest, Queue, Resource, Sweep, Verdict,
};

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

/// Units of a size (`worktree.disk_floor`): decimal and binary multiples of a byte.
const SIZE_UNITS: &[(&str, u64)] = &[
    ("B", 1),
    ("KB", 1_000),
    ("MB", 1_000_000),
    ("GB", 1_000_000_000),
    ("TB", 1_000_000_000_000),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
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
    pub(crate) effort: Option<String>,
    pub(crate) verdict: Verdict,
    pub(crate) policy: Policy,
    pub(crate) build_dirs: BTreeSet<String>,
    pub(crate) landing: DraftLanding,
    pub(crate) mcp_servers: BTreeMap<String, McpServer>,
    pub(crate) worktree: Option<DraftWorktree>,
    pub(crate) sweep: Option<Sweep>,
    pub(crate) auto_pause: Option<bool>,
}

/// `repo.<name>.landing`, with where its mode and `merge.push` are written, for the checks
/// against the worktree and the modes of this version.
#[derive(Debug, Default)]
pub(crate) struct DraftLanding {
    pub(crate) landing: Landing,
    pub(crate) mode_at: Option<Location>,
    pub(crate) push_at: Option<Location>,
    pub(crate) target_at: Option<Location>,
}

/// `repo.<name>.worktree`, its root kept with its key and position for the cross-check against
/// the repository's path.
#[derive(Debug)]
pub(crate) struct DraftWorktree {
    pub(crate) root: Option<PathValue>,
    pub(crate) setup: Option<String>,
    pub(crate) teardown: Option<String>,
    pub(crate) disk_floor: Option<u64>,
    pub(crate) shared_build: Option<DraftSharedBuild>,
}

/// `repo.<name>.worktree.shared_build`, its directory and resource kept with their positions
/// for the cross-checks against the paths and `[resource.<name>]`.
#[derive(Debug)]
pub(crate) struct DraftSharedBuild {
    pub(crate) dir: Option<PathValue>,
    pub(crate) resource: Option<(String, Option<Location>)>,
    pub(crate) env: String,
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
    pub(crate) resources: BTreeMap<String, Resource>,
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
        resources: BTreeMap::new(),
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
            "resource" => {
                for (name, item, at) in reader.named_tables(item, "resource", at) {
                    if let Some(resource) = reader.resource(name, item, at) {
                        draft.resources.insert(name.to_owned(), resource);
                    }
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
                     retention, budget, resource and notify"
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
                "fast_death_sessions" => {
                    set(&mut daemon.fast_death_sessions, self.count(item, &what));
                }
                "fast_death_within" => {
                    set(&mut daemon.fast_death_within, self.duration(item, &what));
                }
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
            effort: None,
            verdict: Verdict::default(),
            policy: Policy::default(),
            build_dirs: BTreeSet::new(),
            landing: DraftLanding::default(),
            mcp_servers: BTreeMap::new(),
            worktree: None,
            sweep: None,
            auto_pause: None,
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
                "effort" => repo.effort = self.effort(item, &what),
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
                        repo.landing = self.landing(table, &what);
                    }
                }
                "mcp_servers" => repo.mcp_servers = self.mcp_servers(item, &what, key_at),
                "worktree" => {
                    if let Some(table) = self.table(item, &what, key_at) {
                        repo.worktree = Some(self.worktree(table, &what, key_at));
                    }
                }
                "sweep" => {
                    if let Some(table) = self.table(item, &what, key_at) {
                        repo.sweep = Some(self.sweep(table, &what));
                    }
                }
                "auto_pause" => repo.auto_pause = self.bool(item, &what),
                _ => self.ignored(key_at, &what),
            }
        }
        // E19: the target is the branch an item is claimed from and lands on, which only worktree
        // mode reads; without a worktree, commits stay on the current branch, as before.
        // Mutant (`mutant_config_target_kept_without_worktree`): the target is kept, silently.
        if repo.worktree.is_none()
            && !cfg!(mutant_config_target_kept_without_worktree)
            && repo.landing.landing.target.take().is_some()
        {
            self.notice(
                repo.landing.target_at,
                format!("`{path}.landing.target` takes effect with `{path}.worktree`; ignored"),
            );
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
                    "`{path}` has a `queue` and no `model`: the default model of its sessions, \
                     e.g. `model = \"opus\"`"
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
                "unready_exit" => {
                    if let Some(code) = self.unready_exit(item, &path) {
                        verdict.unready_exit = Some(code);
                    }
                }
                "unready_backoff" => set(&mut verdict.unready_backoff, self.duration(item, &path)),
                "unready_cap" => set(&mut verdict.unready_cap, self.count(item, &path)),
                _ => self.ignored(key_at, &path),
            }
        }
    }

    /// Reads a policy. An unknown key is a problem, not a notice: a misspelt `protected` must
    /// not silently leave the paths unprotected.
    fn policy(&mut self, table: &dyn TableLike, what: &str) -> Policy {
        let mut protected = BTreeSet::new();
        let mut push = false;
        let mut census = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "protected" => protected = self.entries(item, &path).unwrap_or_default(),
                "push" => set(&mut push, self.bool(item, &path)),
                "census" => {
                    if let Some(table) = self.table(item, &path, key_at) {
                        census = self.census(table, &path, key_at);
                    }
                }
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{what}.{other}`; a policy has `protected`, `push` and \
                         `census`"
                    ),
                ),
            }
        }
        Policy::new(protected, push, census)
    }

    /// Reads `policy.census` (#7): `glob` and the `keep` and `cap` tables of named regexes.
    fn census(
        &mut self,
        table: &dyn TableLike,
        what: &str,
        at: Option<Location>,
    ) -> Option<Census> {
        let mut glob = None;
        let mut keep = BTreeMap::new();
        let mut cap = BTreeMap::new();
        let mut read = true;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "glob" => {
                    let Some((text, text_at)) = self.string(item, &path) else {
                        read = false;
                        continue;
                    };
                    if text.is_empty() || text.starts_with('/') || text.contains('\\') {
                        read = false;
                        self.problem(
                            text_at,
                            format!(
                                "`{path}` is {text:?}; it is a non-empty, repository-relative \
                                 glob with `/` as the separator"
                            ),
                        );
                    } else {
                        glob = Some(text.to_owned());
                    }
                }
                "keep" | "cap" => {
                    let Some(counters) = self.table(item, &path, key_at) else {
                        read = false;
                        continue;
                    };
                    let into = if key == "keep" { &mut keep } else { &mut cap };
                    for (name, item) in counters.iter() {
                        let name_at = self.key_at(counters, name, item);
                        let counter = format!("{path}.{name}");
                        if !valid_name(name) {
                            read = false;
                            self.problem(
                                name_at,
                                format!(
                                    "`{counter}`: a counter's name is ASCII letters, digits, \
                                     `-` and `_`"
                                ),
                            );
                            continue;
                        }
                        match self.string(item, &counter) {
                            Some((regex, _)) if !regex.is_empty() => {
                                into.insert(name.to_owned(), regex.to_owned());
                            }
                            Some((_, regex_at)) => {
                                read = false;
                                self.problem(
                                    regex_at,
                                    format!("`{counter}` is empty; it is a git extended regex"),
                                );
                            }
                            None => read = false,
                        }
                    }
                }
                other => {
                    read = false;
                    self.problem(
                        key_at,
                        format!(
                            "unknown key `{what}.{other}`; a census has `glob`, `keep` and `cap`"
                        ),
                    );
                }
            }
        }
        if let Some(name) = keep.keys().find(|name| cap.contains_key(*name)) {
            self.problem(
                at,
                format!("`{what}`: the counter {name:?} is in both `keep` and `cap`"),
            );
            return None;
        }
        if read && keep.is_empty() && cap.is_empty() {
            self.problem(
                at,
                format!("`{what}` has no counter; name at least one in `keep` or `cap`"),
            );
            return None;
        }
        let Some(glob) = glob else {
            if read {
                self.problem(at, format!("`{what}` has no `glob`"));
            }
            return None;
        };
        read.then(|| Census::new(glob, keep, cap))
    }

    /// Reads `landing` (#8). The checks against the worktree and the modes of this version are
    /// in validation; here a `merge` table with `local` is refused, since only `merge` reads it.
    fn landing(&mut self, table: &dyn TableLike, what: &str) -> DraftLanding {
        let mut draft = DraftLanding::default();
        let mut mode_read = true;
        let mut merge_at = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "mode" => {
                    let Some((mode, at)) = self.string(item, &path) else {
                        mode_read = false;
                        continue;
                    };
                    draft.mode_at = at;
                    if let Some(mode) = LandingMode::parse(mode) {
                        draft.landing.mode = mode;
                    } else {
                        mode_read = false;
                        self.problem(
                            at,
                            format!("`{path}` is {mode:?}; it is \"local\", \"merge\" or \"pr\""),
                        );
                    }
                }
                "target" => {
                    draft.target_at = key_at;
                    draft.landing.target = self.branch(item, &path);
                }
                "merge" => {
                    merge_at = key_at;
                    if let Some(table) = self.table(item, &path, key_at) {
                        self.merge(table, &path, &mut draft);
                    }
                }
                "pr" => {
                    if let Some(table) = self.table(item, &path, key_at) {
                        self.pull_request(table, &path, &mut draft.landing.pr);
                    }
                }
                _ => self.ignored(key_at, &path),
            }
        }
        if mode_read && draft.landing.mode == LandingMode::Local && table.get("merge").is_some() {
            self.problem(
                merge_at,
                format!(
                    "`{what}.merge` is set with the mode \"local\", which always fast-forwards \
                     without a push; it is read with `mode = \"merge\"` only"
                ),
            );
        }
        draft
    }

    /// Reads `landing.merge`. An unknown key is refused.
    fn merge(&mut self, table: &dyn TableLike, what: &str, draft: &mut DraftLanding) {
        let merge = &mut draft.landing.merge;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "method" => {
                    let Some((text, at)) = self.string(item, &path) else {
                        continue;
                    };
                    match crate::MergeMethod::parse(text) {
                        Some(method) => merge.method = method,
                        None => self.problem(
                            at,
                            format!("`{path}` is {text:?}; it is \"ff\", \"squash\" or \"no-ff\""),
                        ),
                    }
                }
                "push" => {
                    draft.push_at = self.at(item.span());
                    set(&mut merge.push, self.bool(item, &path));
                }
                other => self.problem(
                    key_at,
                    format!("unknown key `{what}.{other}`; a merge has `method` and `push`"),
                ),
            }
        }
    }

    /// Reads `landing.pr`. `title` and `describe_session` are a later card's and are ignored
    /// with a notice; any other unknown key is refused.
    fn pull_request(&mut self, table: &dyn TableLike, what: &str, pr: &mut PullRequest) {
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "draft" => set(&mut pr.draft, self.bool(item, &path)),
                "reviewers" => {
                    pr.reviewers = self
                        .words(item, &path)
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                }
                "labels" => pr.labels = self.labels(item, &path).unwrap_or_default(),
                "title" | "describe_session" => self.ignored(key_at, &path),
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{what}.{other}`; a pull request has `draft`, `reviewers` \
                         and `labels`"
                    ),
                ),
            }
        }
    }

    /// Reads `worktree` (#8). An unknown key is refused, as in a policy: a misspelt `root` must
    /// not go unnoticed.
    fn worktree(
        &mut self,
        table: &dyn TableLike,
        what: &str,
        at: Option<Location>,
    ) -> DraftWorktree {
        let mut worktree = DraftWorktree {
            root: None,
            setup: None,
            teardown: None,
            disk_floor: None,
            shared_build: None,
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "root" => worktree.root = self.path(item, &path),
                "setup" => worktree.setup = self.command(item, &path),
                "teardown" => worktree.teardown = self.command(item, &path),
                "disk_floor" => worktree.disk_floor = self.size(item, &path),
                "shared_build" => {
                    if let Some(table) = self.table(item, &path, key_at) {
                        worktree.shared_build = Some(self.shared_build(table, &path, key_at));
                    }
                }
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{what}.{other}`; a worktree has `root`, `setup`, \
                         `teardown`, `disk_floor` and `shared_build`"
                    ),
                ),
            }
        }
        if table.get("root").is_none() {
            self.problem(at, format!("`{what}` has no `root`"));
        }
        worktree
    }

    /// Reads `sweep` (#8, E12). An unknown key is refused: a misspelt `processes` must not leave
    /// the strays running unnoticed.
    fn sweep(&mut self, table: &dyn TableLike, what: &str) -> Sweep {
        let mut sweep = Sweep::default();
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "processes" => {
                    let Some(array) = self.string_array(item, &path) else {
                        continue;
                    };
                    for (name, at) in array {
                        if process_name(name) {
                            sweep.processes.insert(name.to_owned());
                        } else {
                            self.problem(
                                at,
                                format!(
                                    "`{path}` holds {name:?}; a process name is a file name \
                                     without a directory: ASCII letters, digits, `.`, `-` and \
                                     `_`, not only dots"
                                ),
                            );
                        }
                    }
                }
                other => self.problem(
                    key_at,
                    format!("unknown key `{what}.{other}`; a sweep has `processes`"),
                ),
            }
        }
        sweep
    }

    /// Reads `worktree.shared_build` (#8, E1). An unknown key is refused; `dir` and `resource`
    /// are required.
    fn shared_build(
        &mut self,
        table: &dyn TableLike,
        what: &str,
        at: Option<Location>,
    ) -> DraftSharedBuild {
        let mut shared = DraftSharedBuild {
            dir: None,
            resource: None,
            env: crate::DEFAULT_SHARED_BUILD_ENV.to_owned(),
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let path = format!("{what}.{key}");
            match key {
                "dir" => shared.dir = self.path(item, &path),
                "resource" => {
                    if let Some((name, at)) = self.string(item, &path) {
                        shared.resource = Some((name.to_owned(), at));
                    }
                }
                "env" => {
                    let Some((name, at)) = self.string(item, &path) else {
                        continue;
                    };
                    if env_name(name) {
                        name.clone_into(&mut shared.env);
                    } else {
                        self.problem(
                            at,
                            format!(
                                "`{path}` is {name:?}; a variable name is ASCII letters, digits \
                                 and `_`, and does not start with a digit"
                            ),
                        );
                    }
                }
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{what}.{other}`; a shared build has `dir`, `resource` and \
                         `env`"
                    ),
                ),
            }
        }
        for key in ["dir", "resource"] {
            if table.get(key).is_none() {
                self.problem(at, format!("`{what}` has no `{key}`"));
            }
        }
        shared
    }

    /// Reads `[resource.<name>]` (#8). An unknown key is refused.
    fn resource(&mut self, name: &str, item: &Item, at: Option<Location>) -> Option<Resource> {
        let path = format!("resource.{name}");
        let table = item.as_table_like()?;
        let mut kind = None;
        let mut resource = Resource {
            kind: String::new(),
            capacity: 1,
            scope: Scope::Machine,
            artifacts: BTreeMap::new(),
            probe: None,
            probe_timeout: PROBE_TIMEOUT,
        };
        let mut probe_timeout_at = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("{path}.{key}");
            match key {
                "kind" => kind = self.word(item, &what),
                "capacity" => {
                    let capacity = if cfg!(mutant_config_resource_any_capacity) {
                        // Mutant (`mutant_config_resource_any_capacity`): a capacity of 0 is
                        // accepted, a resource no one can ever hold.
                        self.non_negative(item, &what)
                            .and_then(|n| u32::try_from(n).ok())
                    } else {
                        self.count(item, &what)
                    };
                    set(&mut resource.capacity, capacity);
                }
                "scope" => set(&mut resource.scope, self.scope(item, &what)),
                "probe" => {
                    resource.probe = self.command(item, &what);
                    if resource.probe.is_none() && item.is_str() {
                        self.problem(key_at, format!("`{what}` is empty"));
                    }
                }
                "probe_timeout" => {
                    probe_timeout_at = Some(key_at);
                    if let Some(timeout) = self.duration(item, &what) {
                        if timeout > PROBE_TIMEOUT_MAX {
                            self.problem(
                                key_at,
                                format!(
                                    "`{what}` is {}s; a probe runs inside the loop's poll, so it \
                                     is at most {}s",
                                    timeout.as_secs(),
                                    PROBE_TIMEOUT_MAX.as_secs()
                                ),
                            );
                        } else {
                            resource.probe_timeout = timeout;
                        }
                    }
                }
                "artifact" => {
                    for (name, item, at) in self.named_tables(item, &what, key_at) {
                        if let Some(artifact) = self.artifact(&format!("{what}.{name}"), item, at) {
                            resource.artifacts.insert(name.to_owned(), artifact);
                        }
                    }
                }
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{path}.{other}`; a resource has `kind`, `capacity`, \
                         `scope`, `probe`, `probe_timeout` and `artifact`"
                    ),
                ),
            }
        }
        if let Some(at) = probe_timeout_at
            && table.get("probe").is_none()
        {
            self.problem(at, format!("`{path}.probe_timeout` without `probe`"));
        }
        if kind.is_none() && table.get("kind").is_none() {
            self.problem(at, format!("`{path}` has no `kind`"));
        }
        resource.kind = kind?;
        Some(resource)
    }

    /// Reads `[resource.<name>.artifact.<artifact>]` (#44 C). An unknown key (a `push` among
    /// them: the verdict pushes, never the daemon) is refused.
    fn artifact(&mut self, path: &str, item: &Item, at: Option<Location>) -> Option<Artifact> {
        let table = item.as_table_like()?;
        let (mut local, mut device, mut digest) = (None, None, None);
        let mut timeout = ARTIFACT_TIMEOUT;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            let what = format!("{path}.{key}");
            match key {
                "local" => local = self.artifact_local(item, &what),
                "device" => device = self.word(item, &what),
                "digest" => {
                    digest = self.command(item, &what);
                    if digest.is_none() && item.is_str() {
                        self.problem(key_at, format!("`{what}` is empty"));
                    }
                }
                "timeout" => set(&mut timeout, self.duration(item, &what)),
                other => self.problem(
                    key_at,
                    format!(
                        "unknown key `{path}.{other}`; an artifact has `local`, `device`, \
                         `digest` and `timeout`"
                    ),
                ),
            }
        }
        // Mutant (`mutant_config_artifact_digest_optional`): an artifact without `digest` is
        // accepted, a device copy nobody can ask about.
        let required: &[&str] = if cfg!(mutant_config_artifact_digest_optional) {
            &["local", "device"]
        } else {
            &["local", "device", "digest"]
        };
        for &key in required {
            if table.get(key).is_none() {
                self.problem(at, format!("`{path}` has no `{key}`"));
            }
        }
        Some(Artifact {
            local: local?,
            device: device?,
            digest: digest?,
            timeout,
        })
    }

    /// An artifact's `local`: absolute, or relative with `/` separators and no `..` component.
    fn artifact_local(&mut self, item: &Item, what: &str) -> Option<String> {
        let (text, at) = self.string(item, what)?;
        let why = if text.is_empty() {
            Some("it is not empty")
        } else if PathBuf::from(text).is_absolute() {
            None
        } else if text.contains('\\') {
            Some("a relative path is `/`-separated")
        } else if text.split('/').any(|part| part == "..") {
            Some("a relative path has no `..` component")
        } else {
            None
        };
        if let Some(why) = why {
            self.problem(at, format!("`{what}` is {text:?}; {why}"));
            return None;
        }
        Some(text.to_owned())
    }

    fn scope(&mut self, item: &Item, what: &str) -> Option<Scope> {
        let (text, at) = self.string(item, what)?;
        let scope = match text {
            "machine" => Some(Scope::Machine),
            "identity" => Some(Scope::Identity),
            "repository" => Some(Scope::Repository),
            _ => None,
        };
        if scope.is_none() {
            self.problem(
                at,
                format!("`{what}` is {text:?}; it is \"machine\", \"identity\" or \"repository\""),
            );
        }
        scope
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

    /// An effort level of the runner ([`bosun_core::EFFORT_LEVELS`]).
    fn effort(&mut self, item: &Item, what: &str) -> Option<String> {
        let (text, at) = self.string(item, what)?;
        // Mutant (`mutant_config_effort_unchecked`): any word is taken as a level.
        let known = if cfg!(mutant_config_effort_unchecked) {
            !text.is_empty() && text.trim() == text
        } else {
            bosun_core::is_effort_level(text)
        };
        if !known {
            self.problem(
                at,
                format!(
                    "`{what}` is {text:?}; it is one of {}",
                    bosun_core::EFFORT_LEVELS.join(", ")
                ),
            );
            return None;
        }
        Some(text.to_owned())
    }

    /// A branch name, bare, by git's rules ([`branch_name_rule`]).
    fn branch(&mut self, item: &Item, what: &str) -> Option<String> {
        let (text, at) = self.string(item, what)?;
        let why = if text.starts_with("refs/") {
            Some("write the bare branch name, without `refs/heads/`")
        } else if cfg!(mutant_config_target_loose_branch_rules) {
            // Mutant (`mutant_config_target_loose_branch_rules`): the rules before git's.
            if text.is_empty() || text.chars().any(char::is_whitespace) {
                Some("it must be non-empty, with no whitespace")
            } else if text.starts_with('-') {
                Some("it must not start with `-`")
            } else {
                None
            }
        } else {
            branch_name_rule(text)
        };
        if let Some(why) = why {
            self.problem(at, format!("`{what}` is {text:?}; {why}"));
            return None;
        }
        Some(text.to_owned())
    }

    /// A list of labels: each non-empty, with no surrounding spaces; inner spaces are allowed.
    fn labels(&mut self, item: &Item, what: &str) -> Option<Vec<String>> {
        let array = self.string_array(item, what)?;
        let mut out = Vec::new();
        for (text, at) in array {
            if text.is_empty() || text.trim() != text {
                self.problem(
                    at,
                    format!(
                        "`{what}` holds {text:?}; an entry is non-empty, with no surrounding \
                         spaces"
                    ),
                );
            } else {
                out.push(text.to_owned());
            }
        }
        Some(out)
    }

    /// A shell command; an empty one is none.
    fn command(&mut self, item: &Item, what: &str) -> Option<String> {
        self.string(item, what)
            .map(|(value, _)| value.to_owned())
            .filter(|command| !command.trim().is_empty())
    }

    /// A size in bytes, such as `"20GB"` ([`SIZE_UNITS`]).
    fn size(&mut self, item: &Item, what: &str) -> Option<u64> {
        let (text, at) = self.string(item, what)?;
        let value = parse_size(text);
        if value.is_none() {
            self.problem(
                at,
                format!(
                    "`{what}` is {text:?}; expected a positive whole number and a unit (B, KB, \
                     MB, GB, TB, KiB, MiB, GiB, TiB), such as \"20GB\""
                ),
            );
        }
        value
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

    /// `verdict.unready_exit` (#204): an exit code from 2 to 125. 0 is a pass, 1 is what most
    /// failing commands exit with, 126 and 127 are the shell's own cannot-execute and not-found,
    /// and 128 and above report a signal.
    fn unready_exit(&mut self, item: &Item, what: &str) -> Option<u8> {
        let at = self.at(item.span());
        // Mutant (`mutant_config_unready_exit_any_code`): any integer is accepted.
        let range = if cfg!(mutant_config_unready_exit_any_code) {
            i64::MIN..=i64::MAX
        } else {
            2..=125
        };
        let code = item
            .as_integer()
            .filter(|n| range.contains(n))
            .map(|n| u8::try_from(n).unwrap_or(u8::MAX));
        if code.is_none() {
            self.problem(
                at,
                format!(
                    "`{what}` must be a whole number from 2 to 125: 0 is a pass, 1 is any \
                     failure, 126 and 127 are the shell's own, 128 and above are signals; \
                     75 (`EX_TEMPFAIL`) is the suggestion"
                ),
            );
        }
        code
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

/// The rule of `git check-ref-format --branch` that `name` breaks, or `None` for a valid
/// branch name. Pure: parsing the file never runs git.
pub(crate) fn branch_name_rule(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("a branch name is not empty");
    }
    if name == "HEAD" {
        return Some("a branch is not named `HEAD`");
    }
    if name == "@" {
        return Some("a branch is not named `@`");
    }
    if name.starts_with('-') {
        return Some("a branch name does not begin with `-`");
    }
    if let Some(c) = name
        .chars()
        .find(|&c| c.is_ascii_control() || " ~^:?*[\\".contains(c))
    {
        return Some(if c.is_ascii_control() {
            "a branch name has no control character"
        } else if c == ' ' {
            "a branch name has no space"
        } else {
            "a branch name has none of `~`, `^`, `:`, `?`, `*`, `[` and `\\`"
        });
    }
    if name.contains("..") {
        return Some("a branch name has no `..`");
    }
    if name.contains("@{") {
        return Some("a branch name has no `@{`");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Some("a branch name does not begin or end with `/` and has no `//`");
    }
    if name.ends_with('.') {
        return Some("a branch name does not end with `.`");
    }
    // Git compares `.lock` case-sensitively: `x.LOCK` is a valid branch name to it.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    let locked = |part: &str| part.ends_with(".lock");
    if name
        .split('/')
        .any(|part| part.starts_with('.') || locked(part))
    {
        return Some("no component of a branch name begins with `.` or ends with `.lock`");
    }
    None
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

/// The executable file name of a process the sweep may kill: ASCII letters, digits, `.`, `-`
/// and `_`, not empty and not only dots (`.` and `..` name directories).
fn process_name(name: &str) -> bool {
    name.chars().any(|c| c != '.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// An environment variable name: an ASCII identifier.
fn env_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// Parses `<n><unit>` with `n > 0` and the unit one of [`SIZE_UNITS`], into bytes.
fn parse_size(text: &str) -> Option<u64> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (count, unit) = text.split_at(digits);
    let &(_, bytes) = SIZE_UNITS.iter().find(|(name, _)| *name == unit)?;
    let count: u64 = count.parse().ok().filter(|&n| n > 0)?;
    count.checked_mul(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        // GB is 10^9 and GiB is 1024^3 (decided on #124).
        assert_eq!(parse_size("1GB"), Some(1_000_000_000));
        assert_eq!(parse_size("1GiB"), Some(1_073_741_824));
        assert_eq!(parse_size("20GB"), Some(20_000_000_000));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("2TB"), Some(2_000_000_000_000));
        assert_eq!(parse_size("2TiB"), Some(2 << 40));
        assert_eq!(parse_size("1B"), Some(1));
        for bad in [
            "",
            "GB",
            "0GB",
            "20",
            "20 GB",
            "20gb",
            "1.5GB",
            "-1GB",
            "99999999999TB",
        ] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

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
