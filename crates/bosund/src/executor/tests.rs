//! The scenario suite of the loop (#62): `fake-agent` as the agent CLI (behind a `.cmd` wrapper
//! on Windows, as npm installs `claude`), the local-file queue, a real git repository, a real
//! store, the real verdict shell. Nothing touches the user's own configuration, agent or daemon:
//! every path is in a temporary directory and the login probe is a fake.
//!
//! As the agent CLI does, `fake-agent` starts the session's MCP servers from `--mcp-config`:
//! the real `bosun-mcp`, which calls the scene's own endpoint, served with the scene's
//! credentials (#64).
//!
//! Every deadline of a scene (a wait for a file, a state, a drained outbox) is multiplied by
//! `BOSUN_TEST_SLOWDOWN` (#165), a positive integer, 1 when unset: a loaded machine or CI shard
//! sets `BOSUN_TEST_SLOWDOWN=3`. The sleeps that define a scene ("nothing happens within 2 s")
//! do not scale.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bosun_config::{Config, DiskProbe, SystemProbe};
use bosun_core::{Outcome, ParkReason, Reason, SpawnMode, State};
use bosun_identity::{Bundle, ProbeError, TokenLogin};
use bosun_proc::{ChildSpec, NativeSpawner, Spawned, Spawner};
use bosun_queue::projects_v2::fake::{FakeComment, FakeGitHub, FakeItem};
use bosun_queue::projects_v2::{Binding, ProjectRef};
use bosun_queue::{Card, Column, LocalFile, ProjectsV2, Queue};
use bosun_store::{Options as StoreOptions, Store};
use tokio::sync::watch;

use super::*;
use crate::config_watch;
use crate::events::{Events, Options as EventOptions};

mod landing;
mod leases;
mod moved;
mod recovery;
mod refusals;
mod shim;
mod sticky;
mod sweep;
mod timing;
mod worktrees;

/// Removes from `command` every variable it would inherit from a bosun session (#134): each
/// `BOSUN_*` and the command-scope configuration (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*`,
/// `GIT_CONFIG_VALUE_*`) that points git at the session's hooks, so a scene's git behaves the
/// same inside a session and outside one. (`bosun-git`'s tests have their own copy.)
pub(super) fn without_session(command: &mut Command) -> &mut Command {
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("BOSUN_")
            || text == "GIT_CONFIG_COUNT"
            || text.starts_with("GIT_CONFIG_KEY_")
            || text.starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(&name);
        }
    }
    command
}

/// Writes `text` to `path` as an executable script (mode 0755 on Unix).
///
/// On Unix the file is written by a child `sh`, never through a descriptor of this process:
/// the tests run on many threads, and a child forked by another test while this process held
/// the file open for writing keeps that descriptor until its own `exec`, so executing the
/// script meanwhile fails with `ETXTBSY` ("Text file busy").
fn write_executable(path: &Path, text: &str) {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        let mut child = Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        drop(stdin);
        let status = child.wait().unwrap();
        assert!(status.success(), "writing {}: {status}", path.display());
    }
    #[cfg(not(unix))]
    std::fs::write(path, text).unwrap();
}

/// How long a session that asked a blocking question may go on in these scenarios.
const GRACE: Duration = Duration::from_secs(2);

/// The login every identity check sees, and the viewer of the fake GitHub.
const LOGIN: &str = "synthetic-login";

/// The repository whose issues are the cards of the fake Projects v2 board.
const BOARD_REPO: &str = "synthetic-owner/demo";

/// The binaries the scenes run, built next to this test binary.
struct Built {
    agent: PathBuf,
    mcp: PathBuf,
    /// The session shim (#8), which worktree-mode sessions run a copy of.
    shim: PathBuf,
}

/// Runs `cargo build` with `args` and returns the executables of `names`, wherever the target
/// and build directories are.
fn cargo_build(args: &[&str], names: &[&str]) -> Vec<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut build = Command::new(cargo);
    build
        .args(["build", "--locked"])
        .args(args)
        .arg("--message-format=json")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stderr(std::process::Stdio::inherit());
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    let out = build.output().unwrap();
    assert!(out.status.success(), "building {names:?} failed");
    let messages = String::from_utf8_lossy(&out.stdout);
    names
        .iter()
        .map(|name| {
            messages
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|m| m["target"]["name"] == *name)
                .find_map(|m| m["executable"].as_str().map(PathBuf::from))
                .unwrap_or_else(|| panic!("cargo names the {name} executable"))
        })
        .collect()
}

/// `fake-agent`, `bosun-mcp` and `bosun-shim`, built once.
fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        // A child test process (#68) is handed what its parent built.
        if let (Some(agent), Some(mcp), Some(shim)) = (
            std::env::var_os(BUILT_AGENT_VAR),
            std::env::var_os(BUILT_MCP_VAR),
            std::env::var_os(BUILT_SHIM_VAR),
        ) {
            return Built {
                agent: PathBuf::from(agent),
                mcp: PathBuf::from(mcp),
                shim: PathBuf::from(shim),
            };
        }
        let mut both = cargo_build(
            &["--package", "fake-agent", "--package", "bosun-mcp"],
            &["fake-agent", "bosun-mcp"],
        );
        let mut shim = cargo_build(
            &["--package", "bosun-proc", "--bin", "bosun-shim"],
            &["bosun-shim"],
        );
        let (mcp, agent) = (both.remove(1), both.remove(0));
        Built {
            agent,
            mcp,
            shim: shim.remove(0),
        }
    })
}

/// The variables a child test process finds `fake-agent`, `bosun-mcp` and `bosun-shim` in.
const BUILT_AGENT_VAR: &str = "BOSUN_TEST_FAKE_AGENT";
const BUILT_MCP_VAR: &str = "BOSUN_TEST_BOSUN_MCP";
const BUILT_SHIM_VAR: &str = "BOSUN_TEST_BOSUN_SHIM";

fn fake_agent() -> &'static Path {
    &built().agent
}

fn bosun_mcp() -> &'static Path {
    &built().mcp
}

fn bosun_shim() -> &'static Path {
    &built().shim
}

/// A fresh endpoint for one scene's API.
fn scene_endpoint(dir: &Path) -> bosun_client::Endpoint {
    if cfg!(windows) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        bosun_client::Endpoint::new(format!(
            "bosun-test-loop-{}-{n}-{nanos}",
            std::process::id()
        ))
    } else {
        // One per process: the child daemon of the crash tests shares the scene's directory.
        bosun_client::Endpoint::new(dir.join(format!("bosund-{}.sock", std::process::id())))
    }
}

/// A path as a POSIX shell in a verdict reads it.
fn sh_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The token's login: [`LOGIN`], unless the file `login` of the scene says otherwise (a
/// verdict can write it, so the login changes between two checks).
#[derive(Debug)]
struct FixedLogin(PathBuf);

impl TokenLogin for FixedLogin {
    fn login(&self, _host: &str) -> Result<String, ProbeError> {
        Ok(std::fs::read_to_string(&self.0)
            .map_or_else(|_| LOGIN.to_owned(), |text| text.trim().to_owned()))
    }
}

#[derive(Debug)]
struct FixedLogins(PathBuf);

impl Logins for FixedLogins {
    fn probe(&self, _bundle: &Bundle) -> Box<dyn TokenLogin> {
        Box::new(FixedLogin(self.0.clone()))
    }
}

/// The native spawner, counting the agent sessions it spawns (not the verdict shells); in
/// worktree mode it counts those started through the session shim as well.
#[derive(Debug, Default)]
struct Counting {
    spawns: AtomicUsize,
    /// Of `spawns`, those through the shim.
    shimmed: AtomicUsize,
}

impl Counting {
    fn count(&self, spec: &ChildSpec) {
        let agent = spec
            .program
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case("claude"));
        if agent {
            self.spawns.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Spawner for Counting {
    fn spawn(&self, spec: &ChildSpec) -> std::io::Result<Spawned> {
        self.count(spec);
        NativeSpawner.spawn(spec)
    }
}

impl ShimSessions for Counting {
    fn spawn_in(
        &self,
        shim: &bosun_proc::shim::ShimSpawner,
        spec: &ChildSpec,
        dir: &Path,
    ) -> std::io::Result<Spawned> {
        self.count(spec);
        self.shimmed.fetch_add(1, Ordering::SeqCst);
        shim.spawn_in(spec, dir)
    }
}

/// The system's clocks, with a wall clock the test moves forward (a simulated sleep, a park
/// timer run out) while the monotonic clock goes on as it is.
#[derive(Debug)]
struct TestClock {
    system: SystemClock,
    ahead_ms: AtomicI64,
}

impl TestClock {
    fn advance(&self, by: Duration) {
        let ms = i64::try_from(by.as_millis()).unwrap();
        self.ahead_ms.fetch_add(ms, Ordering::SeqCst);
    }

    /// Moves the wall clock to `minutes` past the next local midnight after the real time
    /// (tomorrow at that local time), so a scene runs at a fixed local time of day and never
    /// behind the real clock (#165). Call it before anything is written at the scene's time.
    fn start_at_local(&self, minutes: i64) {
        let real = self.system.wall_ms();
        let target = bosun_proc::next_local_midnight(real) + minutes * 60_000;
        self.ahead_ms.store(target - real, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn wall_ms(&self) -> i64 {
        self.system.wall_ms() + self.ahead_ms.load(Ordering::SeqCst)
    }

    fn mono(&self) -> Duration {
        self.system.mono()
    }
}

/// The wake check's liveness probe: the real one, unless the test declares the child dead (it
/// died in the sleep); counts its calls.
#[derive(Debug, Default)]
struct Probe {
    dead: AtomicBool,
    calls: AtomicUsize,
}

/// The free space the disk floor of worktree mode reads (#8): what the test sets, on every
/// volume; plenty unless a scenario lowers it.
#[derive(Debug)]
struct FakeSpace(AtomicU64);

impl Default for FakeSpace {
    fn default() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}

impl FakeSpace {
    fn set(&self, bytes: u64) {
        self.0.store(bytes, Ordering::SeqCst);
    }
}

impl bosun_git::worktree::FreeSpace for FakeSpace {
    fn free(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

/// Where a scene lives: a temporary directory it owns, or one a parent test process owns (the
/// child daemon of the crash tests, #68).
enum Root {
    Temp(tempfile::TempDir),
    Kept(PathBuf),
}

impl Root {
    fn path(&self) -> &Path {
        match self {
            Self::Temp(dir) => dir.path(),
            Self::Kept(path) => path,
        }
    }
}

/// One scenario's world.
struct Scene {
    dir: Root,
    repo: PathBuf,
    board: PathBuf,
    config_path: PathBuf,
    scripts: PathBuf,
    gitconfig: PathBuf,
    store: Store,
    spawner: Arc<Counting>,
    credentials: crate::agent::Credentials,
    clock: Arc<TestClock>,
    probe: Arc<Probe>,
    /// The endpoint the scene's API is served on, for `bosun-mcp`.
    endpoint: bosun_client::Endpoint,
    /// The MCP server sessions get: the real `bosun-mcp` unless a scenario breaks it.
    mcp: PathBuf,
    /// How long after spawn `bosun-mcp` has to say hello.
    handshake: Duration,
    /// The control state of #67, and the loop's events.
    control: crate::control::Control,
    events: Events,
    /// The fake GitHub of a scene on a Projects v2 board; `None` on the local file.
    projects: Option<Arc<FakeGitHub>>,
    /// Whether [`LOGIN`] is on the identity's author allowlist (#106).
    login_listed: bool,
    /// Worktree mode (#8): `repo.demo.worktree` with its root at `<scene>/wt`; `false`: stage 1.
    worktrees: bool,
    /// The worktree's `setup` command, in worktree mode.
    setup: Option<String>,
    /// The repository's name in the configuration: `demo` unless a scenario of two
    /// repositories names the second otherwise.
    name: String,
    /// More of `repo.<name>.worktree`, after its root and setup (`, shared_build = {...}`).
    worktree_more: String,
    /// More configuration after the repository's (`[resource.<name>]` sections).
    config_more: String,
    /// The lease manager the loop gets; `None`: one of its own, built from the configuration.
    leases: Option<Arc<crate::leases::Leases>>,
    /// The loop's spawns through the session shim; `None`: [`Scene::spawner`].
    sessions: Option<Arc<dyn ShimSessions>>,
    /// The free space the disk floor reads.
    free_space: Arc<FakeSpace>,
    /// The forge of the `pr` landing mode; `None`: a fake of its own nobody looks at.
    forge: Option<Arc<dyn forge::Forge>>,
    /// The daemon's live trees, which the process sweep spares: the loop's, and any a scene
    /// guards as another repository's.
    live: Arc<crate::executor::Live>,
}

impl Scene {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn git(&self, args: &[&str]) -> String {
        let out = without_session(&mut Command::new("git"))
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The configuration file's text, with `protected` as the policy's list.
    fn config_text(&self, protected: &str) -> String {
        let d = self.dir.path();
        format!(
            "[identity.test]\n\
             claude_config_dir = '{claude}'\n\
             database = '{db}'\n\
             github_login = '{LOGIN}'\n\
             gh_config_dir = '{gh}'\n\
             git_config = '{gitconfig}'\n\
             remote_allowlist = ['synthetic-owner']\n\
             author_allowlist = [{authors}]\n\n\
             [repo.{name}]\n\
             path = '{repo}'\n\
             identity = 'test'\n\
             queue = {{ kind = 'local_file', file = '{board}' }}\n\
             model = 'opus'\n\
             verdict = {{ timeout = '60s' }}\n\
             build_dirs = ['target']\n\
             {worktree}\n\
             [repo.{name}.policy]\n\
             protected = [{protected}]\n\
             {more}",
            name = self.name,
            more = self.config_more,
            claude = d.join("claude").display(),
            db = d.join("db").join("bosun.db").display(),
            gh = d.join("gh").display(),
            gitconfig = self.gitconfig.display(),
            repo = self.repo.display(),
            board = self.board.display(),
            worktree = self.worktree_line(),
            authors = self
                .authors()
                .iter()
                .map(|a| format!("'{a}'"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// The `worktree` line of `[repo.demo]` in worktree mode, else nothing.
    fn worktree_line(&self) -> String {
        if !self.worktrees {
            return String::new();
        }
        let setup = self
            .setup
            .as_deref()
            .map_or_else(String::new, |s| format!(", setup = '{s}'"));
        format!(
            "worktree = {{ root = '{}'{setup}{} }}\n",
            self.worktree_root().display(),
            self.worktree_more
        )
    }

    /// The worktree root of a scene in worktree mode.
    fn worktree_root(&self) -> PathBuf {
        self.path("wt")
    }

    async fn new() -> Self {
        Self::on(None).await
    }

    /// A scene in worktree mode (#8): the same configuration plus `repo.demo.worktree`, its
    /// root at `<scene>/wt`, with `setup` as its setup command.
    ///
    /// The scene's repository names its own committer, the identity file's, so the test's own
    /// commits there (a setup script's included) need no global git identity (E26). The
    /// daemon's clone does not inherit it: its commits carry the identity only if the daemon
    /// gives them it.
    async fn with_worktrees(setup: Option<&str>) -> Self {
        let mut scene = Self::new().await;
        scene.git(&["config", "user.name", "Synthetic Agent"]);
        scene.git(&["config", "user.email", "agent@example.com"]);
        scene.worktrees = true;
        scene.setup = setup.map(str::to_owned);
        std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
        scene
    }

    /// A scene whose board is a fake Projects v2 board: one project with the six columns,
    /// holding issues of [`BOARD_REPO`] written by an allowed author.
    async fn on_projects() -> Self {
        let fake = FakeGitHub::new();
        fake.set_viewer(LOGIN);
        fake.add_project(
            "synthetic-owner",
            1,
            &Column::ALL.map(bosun_queue::Column::name),
        );
        Self::on(Some(Arc::new(fake))).await
    }

    async fn on(projects: Option<Arc<FakeGitHub>>) -> Self {
        let scene = Self::build(Root::Temp(tempfile::tempdir().unwrap()), projects).await;
        scene.git(&["init", "-q", "-b", "main"]);
        // The daemon under test runs in this process and its git reads the machine's system
        // and global configuration, which the scene's own git ([`Scene::git`]) does not; the
        // repository's line-ending policy is pinned here, where both read it, so a checkout
        // the daemon writes is compared with the policy it was written with (a runner's
        // system `core.autocrlf=true` would otherwise reach only the daemon's side). A scene
        // that tests a policy sets its own after this.
        scene.git(&["config", "core.autocrlf", "false"]);
        scene.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/synthetic-owner/demo.git",
        ]);
        std::fs::write(scene.repo.join("README.md"), "demo\n").unwrap();
        scene.git(&["add", "README.md"]);
        scene.git(&["commit", "-q", "-m", "Start"]);
        std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
        scene
    }

    /// The scene another test process set up at `root` (#68: the child daemon), with an API
    /// and a store connection of its own.
    async fn reopen(root: PathBuf) -> Self {
        Self::build(Root::Kept(root), None).await
    }

    async fn build(dir: Root, projects: Option<Arc<FakeGitHub>>) -> Self {
        let d = dir.path();
        for sub in ["claude", "db", "gh", "repo", "scripts", "bin", "work"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let gitconfig = d.join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Synthetic Agent\n\temail = agent@example.com\n",
        )
        .unwrap();
        let store = Store::open(d.join("db").join("bosun.db"), StoreOptions::default())
            .await
            .unwrap();
        // The scene's API, holding only the credentials: what `bosun-mcp` calls.
        let credentials = crate::agent::Credentials::default();
        let endpoint = scene_endpoint(d);
        let listener = crate::listen::claim(&endpoint).await.unwrap();
        let control = crate::control::Control::with_token(control::TOKEN, None);
        let api = Arc::new(crate::state::State {
            started_ms: 0,
            identities: BTreeMap::new(),
            credentials: credentials.clone(),
            control: control.clone(),
            leases: Arc::new(crate::leases::Leases::none()),
        });
        tokio::spawn(async move {
            crate::state::serve(listener, &api, std::future::pending()).await;
        });
        Self {
            repo: d.join("repo"),
            board: d.join("board.toml"),
            config_path: d.join("config.toml"),
            scripts: d.join("scripts"),
            gitconfig,
            store,
            spawner: Arc::new(Counting::default()),
            clock: Arc::new(TestClock {
                system: SystemClock::new(),
                ahead_ms: AtomicI64::new(0),
            }),
            probe: Arc::new(Probe::default()),
            credentials,
            endpoint,
            mcp: bosun_mcp().to_owned(),
            handshake: Duration::from_secs(30),
            control,
            events: Events::new("test", EventOptions::default()),
            projects,
            login_listed: true,
            worktrees: false,
            setup: None,
            name: "demo".to_owned(),
            worktree_more: String::new(),
            config_more: String::new(),
            leases: None,
            sessions: None,
            free_space: Arc::new(FakeSpace::default()),
            forge: None,
            live: Arc::default(),
            dir,
        }
    }

    /// The agent CLI: `fake-agent` picking each item's script, behind a `.cmd` shim on Windows.
    fn agent(&self) -> PathBuf {
        let bin = self.path("bin");
        if cfg!(windows) {
            let shim = bin.join("claude.cmd");
            std::fs::write(
                &shim,
                format!(
                    "@echo off\r\n\"{}\" --fake-script-dir \"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
                    fake_agent().display(),
                    self.scripts.display()
                ),
            )
            .unwrap();
            shim
        } else {
            let shim = bin.join("claude");
            write_executable(
                &shim,
                &format!(
                    "#!/bin/sh\nexec '{}' --fake-script-dir '{}' \"$@\"\n",
                    fake_agent().display(),
                    self.scripts.display()
                ),
            );
            shim
        }
    }

    fn script(&self, card: u64, lines: &[String]) {
        let replay = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream/normal-end.jsonl");
        let mut text = lines.join("\n");
        let _ = writeln!(text, "\nreplay {}", replay.display());
        std::fs::write(
            self.scripts.join(format!("{}_{card}.script", self.name)),
            text,
        )
        .unwrap();
    }

    /// The board holds `cards`, in this order: the local file rewritten (atomically, as an
    /// editor that saves atomically does: the loop polls it meanwhile), or the fake's issues
    /// edited (only what changed moves their `updatedAt`) and its project's items set.
    fn write_board(&self, cards: &[Card]) {
        let Some(fake) = &self.projects else {
            let temp = self.board.with_extension("toml.saving");
            std::fs::write(&temp, LocalFile::render(cards)).unwrap();
            // On Windows a rename over a file being read is refused for a moment.
            let mut tries = 0;
            while let Err(e) = std::fs::rename(&temp, &self.board) {
                tries += 1;
                assert!(tries < 100, "the board cannot be replaced: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
            return;
        };
        for card in cards {
            fake.issue(BOARD_REPO, card.id.0, |issue| {
                issue.title.clone_from(&card.title);
                issue.body.clone_from(&card.body);
                issue.author = Some("local".to_owned());
            });
        }
        let items: Vec<FakeItem<'_>> = cards
            .iter()
            .map(|c| FakeItem::Issue(BOARD_REPO, c.id.0, Some(c.column.name())))
            .collect();
        fake.set_items(0, &items);
    }

    /// Takes [`LOGIN`] off the identity's author allowlist (#106), in the configuration and the
    /// board's binding; before [`Scene::start`].
    fn unlist_login(&mut self) {
        self.login_listed = false;
        std::fs::write(&self.config_path, self.config_text("'docs/frozen'")).unwrap();
    }

    /// The identity's author allowlist.
    fn authors(&self) -> Vec<&'static str> {
        if self.login_listed {
            vec!["local", LOGIN]
        } else {
            vec!["local"]
        }
    }

    /// The fake's binding, as the loop's backend connects with it.
    fn binding(&self) -> Binding {
        Binding::new(
            BOARD_REPO,
            [ProjectRef::new("synthetic-owner", 1)],
            self.authors(),
        )
    }

    fn cards(&self) -> Vec<Card> {
        match &self.projects {
            None => LocalFile::new(&self.board).poll(None).unwrap().cards,
            Some(fake) => {
                ProjectsV2::connect(fake.clone(), self.binding())
                    .unwrap()
                    .poll(None)
                    .unwrap()
                    .cards
            }
        }
    }

    /// The repository's visibility: the fake's answer for a Projects v2 scene (private unless
    /// a scenario makes it public); a local-file scene has no GitHub to ask, so it is public.
    fn visibility(&self) -> board::Visibility {
        match &self.projects {
            None => board::Visibility::fixed(true),
            Some(fake) => {
                let fake = fake.clone();
                board::Visibility::new(Box::new(move || {
                    bosun_queue::projects_v2::is_public(&fake, BOARD_REPO)
                        .map_err(|e| e.to_string())
                }))
            }
        }
    }

    /// The loop's board: the local file, or the fake connected at the loop's first poll.
    fn queue(&self) -> board::Shared {
        match &self.projects {
            None => Arc::new(LocalFile::new(&self.board)),
            Some(fake) => {
                let (fake, binding) = (fake.clone(), self.binding());
                Arc::new(board::Lazy::new(Box::new(move || {
                    Ok(
                        Arc::new(ProjectsV2::connect(fake.clone(), binding.clone())?)
                            as board::Shared,
                    )
                })))
            }
        }
    }

    fn config(&self) -> Config {
        bosun_config::load(&self.config_path, &SystemProbe::new()).unwrap()
    }

    /// Starts the loop and the configuration watcher.
    fn start(&self, failpoint: Option<Failpoint>) -> (JoinHandle<()>, JoinHandle<()>) {
        self.start_with(failpoint, self.queue())
    }

    /// Starts the loop on `queue` and the configuration watcher.
    fn start_with(
        &self,
        failpoint: Option<Failpoint>,
        queue: Arc<dyn Queue + Send + Sync>,
    ) -> (JoinHandle<()>, JoinHandle<()>) {
        let (tx, rx) = watch::channel(Arc::new(self.config()));
        let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(SystemProbe::new());
        let watcher = config_watch::spawn(
            self.config_path.clone(),
            tx,
            probe,
            Duration::from_millis(50),
            config_watch::Reported::default(),
        );
        let (options, deps) = self.options_and_deps(failpoint, queue);
        let task = tokio::spawn(Loop::new(options, deps, rx).run());
        (task, watcher)
    }

    /// What a loop of the scene shares, on its own: for a test that acts as an earlier daemon
    /// did (a worktree it made) before the loop starts.
    fn shared(&self) -> Shared {
        let (options, deps) = self.options_and_deps(None, self.queue());
        Shared {
            options,
            deps,
            worktrees: workspace::Registry::default(),
        }
    }

    /// The options and dependencies of the scene's loop.
    fn options_and_deps(
        &self,
        failpoint: Option<Failpoint>,
        queue: Arc<dyn Queue + Send + Sync>,
    ) -> (Options, Deps) {
        let options = Options {
            repo: self.name.clone(),
            daemon_id: "test-host/test-user".to_owned(),
            agent: Some(self.agent()),
            work_dir: self.path("work"),
            poll: Duration::from_millis(50),
            idle_backoff: false,
            recheck: Duration::from_hours(1),
            question_grace: GRACE,
            pr_poll: Duration::from_millis(100),
            mcp: self.mcp.clone(),
            shim: bosun_shim().to_owned(),
            daemon_dir: self.path("state"),
            endpoint: self.endpoint.clone(),
            handshake: self.handshake,
            boot: None,
            failpoint,
        };
        let deps = Deps {
            reader: self.store.reader().clone(),
            writer: self.store.writer().clone(),
            events: self.events.clone(),
            queue,
            visibility: Arc::new(self.visibility()),
            spawner: self.spawner.clone(),
            sessions: self
                .sessions
                .clone()
                .unwrap_or_else(|| self.spawner.clone()),
            power: Arc::new(bosun_proc::FakePower::default()),
            logins: Arc::new(FixedLogins(self.path("login"))),
            credentials: self.credentials.clone(),
            clock: self.clock.clone(),
            alive: {
                let probe = self.probe.clone();
                Arc::new(move |key| {
                    probe.calls.fetch_add(1, Ordering::SeqCst);
                    !probe.dead.load(Ordering::SeqCst) && bosun_proc::is_running(key)
                })
            },
            control: self.control.clone(),
            leases: self.leases.clone().unwrap_or_else(|| {
                Arc::new(
                    crate::leases::Leases::new(
                        &self.config(),
                        &crate::leases::journal_path(&self.path("state").join("bosun.token")),
                    )
                    .unwrap(),
                )
            }),
            live: self.live.clone(),
            free_space: self.free_space.clone(),
            forge: self
                .forge
                .clone()
                .unwrap_or_else(|| Arc::new(landing::FakeForge::default())),
        };
        (options, deps)
    }

    async fn state(&self, card: u64) -> Option<State> {
        self.item(card).await.map(|i| i.state)
    }

    async fn item(&self, card: u64) -> Option<Item> {
        self.store
            .reader()
            .item(&ItemId::new(format!("{}#{card}", self.name)))
            .await
            .unwrap()
            .map(|s| s.item)
    }

    async fn runs(&self, card: u64) -> Vec<bosun_store::RunRow> {
        self.store
            .reader()
            .runs(&ItemId::new(format!("{}#{card}", self.name)))
            .await
            .unwrap()
    }

    /// A stream fixture of `fixtures/claude-stream`.
    fn stream(name: &str) -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream")
            .join(name)
            .display()
            .to_string()
    }
}

fn card(id: u64, title: &str, verdict: &str, prose: &str) -> Card {
    Card {
        id: bosun_queue::ItemId(id),
        title: title.to_owned(),
        column: Column::Ready,
        body: format!("{prose}\n\n```bosun\nverdict = \"{verdict}\"\n```\n"),
        labels: Vec::new(),
        draft: false,
        closed: false,
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

/// The factor of `BOSUN_TEST_SLOWDOWN` for a value of the variable: 1 when it is unset or
/// empty, else the positive integer it holds. Anything else panics, naming the variable and the
/// value.
fn slowdown_of(value: Option<&str>) -> u32 {
    match value {
        None | Some("") => 1,
        Some(text) => match text.parse::<u32>() {
            Ok(factor) if factor > 0 => factor,
            _ => panic!("BOSUN_TEST_SLOWDOWN must be a positive integer, not {text:?}"),
        },
    }
}

/// The factor every scene deadline is multiplied by: `BOSUN_TEST_SLOWDOWN`, read once.
fn slowdown() -> u32 {
    static FACTOR: OnceLock<u32> = OnceLock::new();
    *FACTOR.get_or_init(|| {
        let value = std::env::var_os("BOSUN_TEST_SLOWDOWN")
            .map(|v| v.into_string().expect("BOSUN_TEST_SLOWDOWN is not Unicode"));
        slowdown_of(value.as_deref())
    })
}

/// A scene deadline, stretched by `BOSUN_TEST_SLOWDOWN`.
fn scaled(limit: Duration) -> Duration {
    limit * slowdown()
}

/// Waits until `check` holds; panics after `limit`, stretched by `BOSUN_TEST_SLOWDOWN`.
async fn wait_for(what: &str, limit: Duration, check: impl AsyncFnMut() -> bool) {
    wait_until(what, limit, slowdown(), check).await;
}

/// Waits until `check` holds; panics after `limit` times `factor`.
async fn wait_until(
    what: &str,
    limit: Duration,
    factor: u32,
    mut check: impl AsyncFnMut() -> bool,
) {
    // Mutant (`mutant_test_slowdown_ignored`): the factor is ignored.
    let factor = if cfg!(mutant_test_slowdown_ignored) {
        1
    } else {
        factor
    };
    let deadline = Instant::now() + limit * factor;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until `path` exists. The limit leaves room for a loaded machine: the whole suite runs
/// its scenes in parallel, and a scene of several items reaches its later ones slowly then.
async fn wait_file(path: &Path) {
    wait_for(
        &path.display().to_string(),
        Duration::from_secs(120),
        async || path.exists(),
    )
    .await;
}

fn terminal(state: Option<State>) -> bool {
    matches!(state, Some(State::Done(_) | State::NeedsYou(_)))
}

/// The run of #62's acceptance: seven local-file items (six on the board at start, one added
/// during the run), each ending Done or Needs you with a reason:
///
/// 1. passes on a flaky verdict: Done, its flaky test recorded;
/// 2. prints "done", its verdict fails: Needs you after two attempts, the second prompt
///    carrying the first verdict's output;
/// 3. its verdict passes on the base commit: bounced at claim, never spawned;
/// 4. leaves the file its verdict needs uncommitted: the clean-tree precondition fails it twice
///    (`mutant_git_skip_clean_tree` lets it reach Done);
/// 5. resets below its base: the ancestry precondition fails it twice
///    (`mutant_git_skip_ancestry` lets it reach Done);
/// 6. its verdict is edited on the card during its session: the snapshot's verdict runs
///    (`mutant_loop_live_verdict` runs the edit);
/// 7. added during the run; the policy changes during its session: Needs you
///    (`mutant_loop_ignore_hash_change` lets it reach Done).
///
/// During item 1's session card 4 is edited, card 7 added and cards 4 and 5 swapped; all take
/// effect with no restart. The checkout ends clean on the claim branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seven_items_end_done_or_needs_you_and_board_edits_take_effect() {
    seven_items(Scene::new().await).await;
}

/// #99: the same run on a fake Projects v2 board, connected at the loop's first poll: every
/// item ends as it does on the local file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seven_items_on_a_projects_v2_board_end_as_on_the_local_file() {
    let scene = Scene::on_projects().await;
    let fake = scene.projects.clone().unwrap();
    seven_items(scene).await;
    // The claims went through the board: each claimed card got this daemon's marker.
    for n in 1..=7 {
        let issue = fake.issue_now(BOARD_REPO, n);
        assert!(
            issue
                .comments
                .iter()
                .any(|c| c.body.contains("test-host/test-user")),
            "card {n} carries no claim marker: {issue:?}"
        );
    }
}

#[allow(clippy::too_many_lines)]
async fn seven_items(scene: Scene) {
    let flag = |name: &str| scene.path(name);
    let p = |name: &str| flag(name).display().to_string();
    let count1 = sh_path(&flag("count1"));
    scene.write_board(&[
        card(
            1,
            "Flaky",
            &format!(
                "n=$(cat {count1} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count1}; \
                 if [ $n -eq 2 ]; then echo 'test a::b ... FAILED'; exit 1; fi; \
                 [ $n -ge 2 ] && test -f one.txt"
            ),
            "Item one.",
        ),
        card(2, "Says done", "echo verdict-says-no; false", "Item two."),
        card(3, "Passes on base", "true", "Item three."),
        card(4, "Uncommitted", "test -f loose.txt", "Item four."),
        card(5, "Below base", "test -f five.txt", "Item five."),
        card(6, "Edited verdict", "test -f six.txt", "Item six."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            format!("argv {}", p("argv2")),
            format!("stdin {}", p("prompt2")),
            "print done".to_owned(),
            "git commit --allow-empty -q -m two".to_owned(),
        ],
    );
    scene.script(3, &[format!("write {} x", p("spawned3"))]);
    scene.script(
        4,
        &[
            "git commit --allow-empty -q -m four".to_owned(),
            "write loose.txt x".to_owned(),
        ],
    );
    scene.script(
        5,
        &[
            "git reset -q --hard HEAD~1".to_owned(),
            "commit five.txt five".to_owned(),
        ],
    );
    scene.script(
        6,
        &[
            format!("write {} x", p("flag6")),
            format!("wait {}", p("release6")),
            "commit six.txt six".to_owned(),
        ],
    );
    scene.script(
        7,
        &[
            format!("write {} x", p("flag7")),
            format!("wait {}", p("release7")),
            "commit seven.txt seven".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);

    // During item 1's session: an edit, an addition and a reorder.
    wait_file(&flag("flag1")).await;
    let mut cards = scene.cards();
    let at = |cards: &[Card], id: u64| cards.iter().position(|c| c.id.0 == id).unwrap();
    let four = at(&cards, 4);
    cards[four].body = cards[four]
        .body
        .replace("Item four.", "Item four, edited while item one ran.");
    let (four, five) = (at(&cards, 4), at(&cards, 5));
    cards.swap(four, five);
    cards.push(card(
        7,
        "Policy changes",
        "test -f seven.txt",
        "Item seven.",
    ));
    scene.write_board(&cards);
    std::fs::write(flag("release1"), "").unwrap();

    // During item 6's session its card's verdict is edited.
    wait_file(&flag("flag6")).await;
    let mut cards = scene.cards();
    let six = at(&cards, 6);
    cards[six].body = cards[six].body.replace("test -f six.txt", "false");
    scene.write_board(&cards);
    std::fs::write(flag("release6"), "").unwrap();

    // During item 7's session the policy changes.
    wait_file(&flag("flag7")).await;
    std::fs::write(
        &scene.config_path,
        scene.config_text("'docs/frozen', 'tests/harness'"),
    )
    .unwrap();
    let deadline = Instant::now() + scaled(Duration::from_secs(15));
    while Instant::now() < deadline && !terminal(scene.state(7).await) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A loop that missed the change gets its session released, and must not reach Done.
    std::fs::write(flag("release7"), "").unwrap();

    let mut states = BTreeMap::new();
    wait_for("every item to end", Duration::from_secs(180), async || {
        for n in 1..=7 {
            states.insert(n, scene.state(n).await);
        }
        states.values().all(|s| terminal(*s))
    })
    .await;
    task.abort();
    watcher.abort();

    let id = |n: u64| ItemId::new(format!("demo#{n}"));
    let reader = scene.store.reader();
    let exhausted = Some(State::NeedsYou(Reason::AttemptsExhausted));
    assert_eq!(states[&1], Some(State::Done(Outcome::Flaky)), "item 1");
    assert_eq!(states[&2], exhausted, "item 2");
    assert_eq!(states[&3], Some(State::NeedsYou(Reason::Lint)), "item 3");
    assert_eq!(
        states[&4], exhausted,
        "item 4: an uncommitted file fails the clean tree"
    );
    assert_eq!(
        states[&5], exhausted,
        "item 5: a reset below the base fails ancestry"
    );
    assert_eq!(states[&6], Some(State::Done(Outcome::Pass)), "item 6");
    assert_eq!(
        states[&7],
        Some(State::NeedsYou(Reason::ConfigChanged)),
        "item 7: the policy changed during its session"
    );

    // Claimed in board order, the reorder and the addition included.
    let mut claimed = Vec::new();
    for n in 1..=7 {
        let ts = reader
            .transitions(&id(n))
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.cause == "claim")
            .map(|t| (t.ts_ms, t.seq));
        claimed.push((ts.expect("every card was claimed"), n));
    }
    claimed.sort_unstable();
    let order: Vec<u64> = claimed.iter().map(|(_, n)| *n).collect();
    assert_eq!(order, [1, 2, 3, 5, 4, 6, 7], "claim order");
    // The edit took effect: item 4 ran from the edited body.
    let snapshot = reader.snapshot(&id(4)).await.unwrap().unwrap();
    assert!(
        snapshot.body.contains("edited while item one ran"),
        "{}",
        snapshot.body
    );

    // Item 3 was bounced by its verdict on the base commit and never spawned.
    assert_eq!(reader.runs(&id(3)).await.unwrap().len(), 0);
    assert!(!flag("spawned3").exists());

    // Item 2: two attempts; the dropped one is stored as failed; the second prompt carries the
    // first verdict's output; the command loads no user settings, through the shim intact.
    let attempts = reader.attempts(&id(2)).await.unwrap();
    let states2: Vec<&str> = attempts.iter().map(|a| a.attempt.state.as_str()).collect();
    assert_eq!(states2, ["fail", "fail"]);
    let prompt = std::fs::read_to_string(flag("prompt2")).unwrap();
    assert!(prompt.contains("verdict-says-no"), "{prompt}");
    let argv: Vec<String> = std::fs::read_to_string(flag("argv2"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let after = |flag: &str| {
        argv.iter()
            .position(|a| a == flag)
            .map(|i| argv[i + 1].as_str())
    };
    assert_eq!(after("--setting-sources"), Some("project"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "Bash(git push:*)"), "{argv:?}");

    // Item 1: its flaky test, and its session's commits linked to it.
    let flaky = reader.flaky_tests(&id(1)).await.unwrap();
    assert_eq!(flaky, [(1, "a::b".to_owned())]);
    let runs = reader.runs(&id(1)).await.unwrap();
    assert!(
        runs.iter()
            .all(|r| r.base_sha.is_some() && r.head_sha.is_some()),
        "{runs:?}"
    );

    // Item 6 ran the snapshot's verdict, not the edited card's.
    let verdicts = reader.verdict_runs(&id(6)).await.unwrap();
    assert!(
        verdicts.iter().all(|v| v.command == "test -f six.txt"),
        "{verdicts:?}"
    );

    // The checkout ends clean on the claim branch, with the Done items' work only.
    assert_eq!(scene.git(&["status", "--porcelain"]), "");
    assert_eq!(scene.git(&["symbolic-ref", "--short", "HEAD"]), "main");
    for (file, kept) in [
        ("one.txt", true),
        ("six.txt", true),
        ("five.txt", false),
        ("loose.txt", false),
        ("seven.txt", false),
    ] {
        assert_eq!(scene.repo.join(file).exists(), kept, "{file}");
    }
    // The work of an item that did not end Done is set aside, not lost.
    let aside = scene.git(&["for-each-ref", "--format=%(refname)", "refs/bosun/items/"]);
    assert!(aside.contains("refs/bosun/items/demo-5"), "{aside}");
}

/// Every transition is written before its effects: when the store refuses the transition that
/// carries the claim, the board is not written and nothing is spawned
/// (`mutant_loop_effect_before_transition` performs the claim first: the card moves to In
/// progress and gets its claim marker with no row behind them). The claim is the effect
/// observed because the store itself refuses a session's run row without its attempt, which
/// only the transition creates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_transition_leaves_no_effect_without_its_row() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let refused = Arc::new(AtomicUsize::new(0));
    let seen = refused.clone();
    let failpoint: Failpoint = Arc::new(move |t: &bosun_core::Transition| {
        let claims = t
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Claim));
        if claims {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        claims
    });
    let (task, watcher) = scene.start(Some(failpoint));
    // The loop halts once the store refuses a transition.
    tokio::time::timeout(scaled(Duration::from_secs(60)), task)
        .await
        .expect("the loop stops after a refused transition")
        .unwrap();
    watcher.abort();
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    // An effect performed before its row would show up here.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let cards = scene.cards();
    assert_eq!(
        (cards[0].column, cards[0].comments.len()),
        (Column::Ready, 0),
        "the board was written without the claim's transition"
    );
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let id = ItemId::new("demo#1");
    assert_eq!(scene.store.reader().runs(&id).await.unwrap().len(), 0);
    assert_eq!(scene.state(1).await, Some(State::Ready));
}

/// The session's credential, as its MCP configuration carries it to `bosun-mcp`.
fn credential_of_session(work: &Path) -> String {
    let sessions = std::fs::read_dir(work.join("sessions")).unwrap();
    let mut found = Vec::new();
    for dir in sessions {
        let text = std::fs::read_to_string(dir.unwrap().path().join("mcp.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();
        let env = &config["mcpServers"]["bosun"]["env"];
        found.push(env[session::CREDENTIAL_VAR].as_str().unwrap().to_owned());
    }
    assert_eq!(found.len(), 1, "one session, one credential");
    found.remove(0)
}

/// Sends one request on `conn` and returns its response.
async fn rpc(
    state: &crate::state::State,
    conn: &mut crate::api::Connection,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let line = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    match crate::api::handle_line(state, conn, &line.to_string()).await {
        crate::api::Answer::Reply(Some(response)) => serde_json::to_value(response).unwrap(),
        other => panic!("{method}: {other:?}"),
    }
}

fn error_code(response: &serde_json::Value) -> Option<i64> {
    response["error"]["code"].as_i64()
}

/// The `agent.*` methods of #63 against a running session of the loop:
///
/// - the session's credential is bound to its item: a call naming another item is refused
///   (`mutant_agent_any_item` accepts it), and it cannot call `tokens.*`, on its own connection
///   or carried to another;
/// - a summary that says "verdict passed" changes no state;
/// - of two summaries the report data holds the second, with the HEAD at its call
///   (`mutant_agent_first_summary_wins` is the store's own test);
/// - the note after the cap is refused and recorded once, as refused (`mutant_agent_no_rate_cap`
///   accepts it);
/// - a blocking question moves the item to Needs you and the session is ended within the grace
///   period; then the credential is revoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn agent_calls_are_bound_capped_recorded_and_a_blocking_question_ends_the_session() {
    use bosun_api::rpc::code;
    use bosun_store::AgentCallKind;

    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Asks", "test -f never.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit a.txt a".to_owned(),
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit b.txt b".to_owned(),
            format!("write {} x", p("flag2")),
            format!("wait {}", p("release2")),
        ],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_file(&scene.path("flag1")).await;
    let credential = credential_of_session(&scene.path("work"));
    assert!(credential.starts_with("bosun-agent-"), "{credential}");
    let state = crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
        leases: Arc::new(crate::leases::Leases::none()),
    };
    let mut conn = crate::api::Connection::default();
    let own = |extra: serde_json::Value| {
        let mut params = serde_json::json!({"credential": credential, "item": "demo#1"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };

    let hello = rpc(&state, &mut conn, "agent.hello", own(serde_json::json!({}))).await;
    assert_eq!(hello["result"]["item"], "demo#1", "{hello}");
    assert_eq!(hello["result"]["attempt"], 1, "{hello}");
    assert_eq!(hello["result"]["identity"], "test", "{hello}");

    // Bound to its item.
    let forged = rpc(
        &state,
        &mut conn,
        "agent.note",
        serde_json::json!({"credential": credential, "item": "demo#2", "text": "forged"}),
    )
    .await;
    assert_eq!(
        error_code(&forged),
        Some(code::UNAUTHORIZED.into()),
        "{forged}"
    );
    // `agent.*` only: on its own connection, or carried to a fresh one.
    let tokens = rpc(
        &state,
        &mut conn,
        "tokens.by_model",
        serde_json::json!({"identity": "test"}),
    )
    .await;
    assert_eq!(
        error_code(&tokens),
        Some(code::UNAUTHORIZED.into()),
        "{tokens}"
    );
    let mut fresh = crate::api::Connection::default();
    let carried = rpc(
        &state,
        &mut fresh,
        "tokens.by_model",
        serde_json::json!({"identity": "test", "credential": credential}),
    )
    .await;
    assert_eq!(
        error_code(&carried),
        Some(code::UNAUTHORIZED.into()),
        "{carried}"
    );
    // Without a credential `agent.*` admits nothing.
    let bare = rpc(
        &state,
        &mut fresh,
        "agent.note",
        serde_json::json!({"credential": "bosun-agent-00", "item": "demo#1", "text": "x"}),
    )
    .await;
    assert_eq!(error_code(&bare), Some(code::UNAUTHORIZED.into()), "{bare}");

    // A summary that claims the verdict passed moves nothing.
    let transitions = scene.store.reader().transitions(&id).await.unwrap().len();
    let claim = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "verdict passed", "decisions": ["done"]})),
    )
    .await;
    assert!(claim["result"].is_object(), "{claim}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scene.state(1).await, Some(State::Running));
    assert_eq!(
        scene.store.reader().transitions(&id).await.unwrap().len(),
        transitions
    );

    // Two summaries, each at its HEAD.
    let first = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "first", "followups": ["later"]})),
    )
    .await;
    let head1 = scene.git(&["rev-parse", "HEAD"]);
    assert_eq!(first["result"]["head_sha"], head1.as_str(), "{first}");
    std::fs::write(scene.path("release1"), "x").unwrap();
    wait_file(&scene.path("flag2")).await;
    let second = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "second", "decisions": ["kept"]})),
    )
    .await;
    let head2 = scene.git(&["rev-parse", "HEAD"]);
    assert_ne!(head1, head2);
    assert_eq!(second["result"]["head_sha"], head2.as_str(), "{second}");
    let summary = scene
        .store
        .reader()
        .summary(&id, 1)
        .await
        .unwrap()
        .expect("a summary");
    assert_eq!(summary.body, "second");
    assert_eq!(summary.head_sha.as_deref(), Some(head2.as_str()));
    assert_eq!(
        summary.kind,
        AgentCallKind::Summary {
            decisions: vec!["kept".to_owned()],
            followups: Vec::new(),
        }
    );

    // Notes up to the cap, then one over it: refused, and recorded once, as refused.
    let cap = crate::agent::cap(crate::agent::Tool::Note);
    for n in 0..cap {
        let note = rpc(
            &state,
            &mut conn,
            "agent.note",
            own(serde_json::json!({"text": format!("note {n}")})),
        )
        .await;
        assert!(note["result"].is_object(), "{note}");
    }
    let over = rpc(
        &state,
        &mut conn,
        "agent.note",
        own(serde_json::json!({"text": "over the cap"})),
    )
    .await;
    assert_eq!(error_code(&over), Some(code::RATE_CAPPED.into()), "{over}");
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    let notes: Vec<_> = calls
        .iter()
        .filter(|c| c.kind == AgentCallKind::Note)
        .collect();
    assert_eq!(notes.len(), cap as usize + 1, "every call is a row");
    let refused: Vec<_> = notes.iter().filter(|c| c.refused).collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].body, "over the cap");

    // A blocking question: Needs you, and the session ends within the grace period.
    let asked = std::time::SystemTime::now();
    let question = rpc(
        &state,
        &mut conn,
        "agent.ask_question",
        own(serde_json::json!({"text": "which way?", "blocking": true})),
    )
    .await;
    assert!(question["result"].is_object(), "{question}");
    wait_for("needs you", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Question))
    })
    .await;
    wait_for("the session's end", Duration::from_secs(30), async || {
        let runs = scene.store.reader().runs(&id).await.unwrap();
        runs.last().is_some_and(|r| r.ended_ms.is_some())
    })
    .await;
    let ended = asked.elapsed().unwrap();
    assert!(
        ended < GRACE + Duration::from_secs(5),
        "the session ended {ended:?} after the question"
    );
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    assert!(
        calls
            .iter()
            .any(|c| c.kind == AgentCallKind::Question { blocking: true }
                && c.body == "which way?"
                && !c.refused)
    );
    // The session is gone, and its credential with it.
    let revoked = rpc(
        &state,
        &mut conn,
        "agent.note",
        own(serde_json::json!({"text": "late"})),
    )
    .await;
    assert_eq!(
        error_code(&revoked),
        Some(code::UNAUTHORIZED.into()),
        "{revoked}"
    );
    assert!(!scene.credentials.live_for(&id));

    task.abort();
    watcher.abort();
}

/// Waits until the outbox of the scene's repository is empty; names the rows left when it
/// times out.
async fn drained(scene: &Scene) {
    let deadline = Instant::now() + scaled(Duration::from_secs(60));
    loop {
        let rows = outbox::pending(scene.store.reader(), "demo").await.unwrap();
        if rows.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the outbox to drain; at {} ms: {rows:#?}",
            scene.clock.wall_ms()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits for item `card` to end; at most `limit`.
async fn ended(scene: &Scene, card: u64, limit: Duration) -> State {
    let mut state = None;
    wait_for(&format!("item {card} to end"), limit, async || {
        state = scene.state(card).await;
        terminal(state)
    })
    .await;
    state.unwrap()
}

fn modes(runs: &[bosun_store::RunRow]) -> Vec<(u32, SpawnMode)> {
    runs.iter().map(|r| (r.attempt, r.mode)).collect()
}

/// #66: a limit message parks the item, the park timer (the default backoff, then a parsed
/// reset) runs out on the loop's clock, and the session is resumed by id; the attempt counter
/// does not move (`mutant_loop_limit_counts` reports the limit as a crash: the second one fails
/// the attempt).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_parks_and_resumes_without_counting() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Limited", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!(
                "once {} replay {}",
                m("m1"),
                Scene::stream("limit-no-reset.jsonl")
            ),
            format!("once {} exit 1", m("m2")),
            format!(
                "once {} replay {}",
                m("m3"),
                Scene::stream("limit-reset.jsonl")
            ),
            format!("once {} exit 1", m("m4")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);

    // Parked for the default backoff: it stays parked while the clock does not move.
    let mut park = None;
    wait_for(
        "the limit to park the item",
        Duration::from_secs(30),
        async || {
            park = match scene.state(1).await {
                Some(State::Parked(park)) => Some(park),
                _ => None,
            };
            park.is_some()
        },
    )
    .await;
    let park = park.unwrap();
    assert_eq!(park.reason, ParkReason::Limit);
    let until = park.until_ms.expect("a limit parks until a time");
    assert!(
        until > scene.clock.wall_ms() + 14 * 60_000,
        "the default backoff: {park:?}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(scene.state(1).await, Some(State::Parked(_))));
    assert_eq!(
        scene.runs(1).await.len(),
        1,
        "no resume before the park ends"
    );

    scene.clock.advance(Duration::from_mins(16));
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!(
        (item.attempts, item.failed),
        (1, 0),
        "the attempt counter did not move"
    );
    assert_eq!(item.uncounted, 2);
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        [
            (1, SpawnMode::Fresh),
            (1, SpawnMode::Resume),
            (1, SpawnMode::Resume)
        ]
    );
    assert!(
        runs.iter().all(|r| r.session_key == runs[0].session_key),
        "resumed by id: {runs:?}"
    );
    let parks = scene
        .store
        .reader()
        .transitions(&ItemId::new("demo#1"))
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.to == "parked")
        .count();
    assert_eq!(parks, 2);
}

/// #66: a resume that fails (the CLI finds no conversation) resets the tree to the last commit
/// and starts a fresh session of the same attempt, uncounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_resume_starts_a_fresh_session_from_the_last_commit() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(
        1,
        "Resume fails",
        "test -f one.txt && test -f kept.txt",
        "Item one.",
    )]);
    scene.script(
        1,
        &[
            format!("once {} commit kept.txt kept", m("m1")),
            format!("once {} write loose.txt uncommitted", m("m2")),
            format!(
                "once {} replay {}",
                m("m3"),
                Scene::stream("limit-reset.jsonl")
            ),
            format!("once {} exit 1", m("m4")),
            format!(
                "once {} replay {}",
                m("m5"),
                Scene::stream("crash-error-result.jsonl")
            ),
            format!("once {} exit 1", m("m6")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    // The loose file would fail the clean-tree precondition had the tree not been reset.
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "uncounted");
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        [
            (1, SpawnMode::Fresh),
            (1, SpawnMode::Resume),
            (1, SpawnMode::Reset)
        ]
    );
    assert_eq!(runs[1].session_key, runs[0].session_key);
    assert_ne!(runs[2].session_key, runs[0].session_key, "a fresh session");
    // Reset to the last commit, not to the base.
    assert!(scene.repo.join("kept.txt").exists());
    assert!(!scene.repo.join("loose.txt").exists());
}

/// #66: an agent that prints a fake limit forever cannot park its item forever: the cap of
/// uncounted parks sends it to Needs you, well inside the timeout (`mutant_loop_no_park_cap`
/// does not apply the cap: the test fails on its timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fake_limit_forever_ends_in_needs_you_at_the_cap() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Always limited", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("replay {}", Scene::stream("limit-reset.jsonl")),
            "exit 1".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::ParkCap));
    let cap = scene.config().daemon.park_cap;
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0));
    assert_eq!(scene.runs(1).await.len(), usize::try_from(cap).unwrap() + 1);
}

/// #66: an expired authentication is Needs you at once, with no retry
/// (`mutant_loop_auth_retries` retries it like a crash).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_authentication_is_needs_you_with_no_retry() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Logged out", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("replay {}", Scene::stream("auth-expired.jsonl")),
            "exit 1".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    // Time for a retry to show.
    tokio::time::sleep(Duration::from_millis(500)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::Auth));
    assert_eq!(scene.runs(1).await.len(), 1);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
}

/// #66: a simulated 20-minute sleep (the wall clock jumps, the monotonic clock does not). Item
/// 1's child died in it: a crash of the attempt, retried once uncounted by resuming. Item 2's
/// child lives: its session runs on and ends normally, with no second session
/// (`mutant_loop_skip_wake_check` never revalidates: item 1 waits on its dead session).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_sleep_a_dead_child_is_retried_once_uncounted_and_a_live_one_runs_on() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "Dies asleep", "test -f one.txt", "Item one."),
        card(2, "Sleeps through", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("once {} wait {}", p("m1"), p("never")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            format!("write {} x", p("flag2")),
            format!("wait {}", p("release2")),
            "commit two.txt two".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let checks = || scene.probe.calls.load(Ordering::SeqCst);

    // Item 1: the child is declared dead in the sleep.
    wait_file(&scene.path("flag1")).await;
    scene.probe.dead.store(true, Ordering::SeqCst);
    scene.clock.advance(Duration::from_mins(20));
    wait_for("the wake check", Duration::from_secs(20), async || {
        checks() > 0
    })
    .await;
    scene.probe.dead.store(false, Ordering::SeqCst);
    let state = ended(&scene, 1, Duration::from_secs(30)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed, item.uncounted), (1, 0, 1));
    assert_eq!(
        modes(&scene.runs(1).await),
        [(1, SpawnMode::Fresh), (1, SpawnMode::Resume)]
    );

    // Item 2: the child lives through the sleep.
    wait_file(&scene.path("flag2")).await;
    let before = checks();
    scene.clock.advance(Duration::from_mins(20));
    wait_for("the wake check", Duration::from_secs(20), async || {
        checks() > before
    })
    .await;
    std::fs::write(scene.path("release2"), "").unwrap();
    let state = ended(&scene, 2, Duration::from_secs(30)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    assert_eq!(modes(&scene.runs(2).await), [(1, SpawnMode::Fresh)]);
    assert_eq!(scene.item(2).await.unwrap().uncounted, 0);
}

/// The flag after `--mcp-config` in a session's argv, as `fake-agent`'s `argv` wrote it.
fn mcp_config_of(argv: &Path) -> PathBuf {
    let argv = std::fs::read_to_string(argv).unwrap();
    let mut words = argv.lines();
    words.find(|w| *w == "--mcp-config").expect("--mcp-config");
    PathBuf::from(words.next().expect("its file"))
}

/// The configuration is readable by the user only.
fn assert_private(path: &Path) {
    #[cfg(windows)]
    {
        let file = std::fs::File::open(path).unwrap();
        bosun_proc::pipe::OwnerOnly::new()
            .unwrap()
            .verify(&file)
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", path.display());
    }
}

/// #64, through the real `bosun-mcp` that `fake-agent` starts from the session's MCP
/// configuration:
///
/// - `tools/list` names exactly the three tools (`mutant_mcp_extra_tool` is `bosun-mcp`'s own
///   test);
/// - a tool call reaches the store through the API, as a row of the session's attempt;
/// - the configuration is outside the working tree (`mutant_mcp_config_in_tree` writes it
///   into the tree), readable by the user only, and gone after the session; the tree stays
///   clean and the item reaches Done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_call_reaches_the_store_through_bosun_mcp_and_the_configuration_stays_private() {
    use bosun_store::AgentCallKind;

    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Talks", "test -f done.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("argv {}", p("argv.txt")),
            format!("mcp-tools bosun {}", p("tools.txt")),
            r#"mcp bosun note {"text": "from the agent"}"#.to_owned(),
            format!("write {} x", p("flag")),
            format!("wait {}", p("release")),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_file(&scene.path("flag")).await;

    assert_eq!(
        std::fs::read_to_string(scene.path("tools.txt")).unwrap(),
        "ask_question\nsubmit_summary\nnote\n"
    );
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].kind, AgentCallKind::Note);
    assert_eq!(calls[0].body, "from the agent");
    assert_eq!(calls[0].attempt, 1);
    assert!(!calls[0].refused);
    assert_eq!(
        calls[0].head_sha.as_deref(),
        Some(scene.git(&["rev-parse", "HEAD"]).as_str())
    );

    let config = mcp_config_of(&scene.path("argv.txt"));
    let repo = scene.repo.canonicalize().unwrap();
    let canonical = config.canonicalize().unwrap();
    assert!(
        !canonical.starts_with(&repo),
        "{} is in the working tree",
        config.display()
    );
    assert_private(&config);
    assert_eq!(scene.git(&["status", "--porcelain", "--ignored"]), "");

    std::fs::write(scene.path("release"), "").unwrap();
    wait_for("done", Duration::from_secs(60), async || {
        matches!(scene.state(1).await, Some(State::Done(_)))
    })
    .await;
    assert!(
        !config.exists(),
        "{} outlived its session",
        config.display()
    );
    task.abort();
    watcher.abort();
}

/// #64: a session whose `bosun-mcp` cannot start never says hello. It is killed at the
/// handshake's timeout and started again, uncounted (the attempt stays 1, nothing failed); the
/// second miss is Needs you (`handshake`). `mutant_mcp_skip_handshake` lets the session run on
/// without its tools, and the item reaches Done instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_whose_mcp_server_cannot_start_is_retried_once_then_needs_you() {
    let mut scene = Scene::new().await;
    scene.mcp = scene.path("bin").join("no-such-bosun-mcp");
    scene.handshake = Duration::from_secs(2);
    scene.write_board(&[card(1, "Toolless", "test -f done.txt", "Item one.")]);
    // Longer than the handshake: the timeout, not the end of the session, finds the miss.
    scene.script(
        1,
        &["sleep 5000".to_owned(), "commit done.txt done".to_owned()],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_for("an end", Duration::from_secs(60), async || {
        matches!(
            scene.state(1).await,
            Some(State::NeedsYou(_) | State::Done(_))
        )
    })
    .await;
    assert_eq!(
        scene.state(1).await,
        Some(State::NeedsYou(Reason::Handshake))
    );
    let stored = scene.store.reader().item(&id).await.unwrap().unwrap().item;
    assert_eq!(
        stored.attempt.as_ref().map(|a| a.number),
        Some(1),
        "{stored:?}"
    );
    assert_eq!(stored.failed, 0, "{stored:?}");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 2);
    let runs = scene.store.reader().runs(&id).await.unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs.iter().all(|r| r.exit == Some(bosun_core::Exit::Crash)),
        "{runs:?}"
    );
    // Killed at the timeout, well before the script's end.
    for run in &runs {
        let lasted = run.ended_ms.unwrap() - run.started_ms;
        assert!(lasted < 4_500, "a session lasted {lasted} ms");
    }
    task.abort();
    watcher.abort();
}

/// A card whose contract allows the protected path `allowed`.
fn card_allowing(id: u64, title: &str, verdict: &str, allowed: &str) -> Card {
    let mut card = card(id, title, verdict, title);
    card.body = card.body.replace(
        "verdict = ",
        &format!("allow-protected = [\"{allowed}\"]\nverdict = "),
    );
    card
}

/// The values of trailer `key` on commit `rev` of the scene's repository.
fn trailer(scene: &Scene, rev: &str, key: &str) -> Vec<String> {
    let out = scene.git(&[
        "log",
        "-1",
        &format!("--format=%(trailers:key={key},valueonly)"),
        rev,
    ]);
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// #99: the guards of #59 and the trailers in the loop. Six items on a repository that ignores
/// `*.log`, whose policy protects `docs/frozen`:
///
/// 1. leaves a new ignored `hidden.log` its verdict needs: Needs you (`guard_ignored_file`),
///    and the file is not deleted (`mutant_git_ignored_is_warning` lets it reach Done);
/// 2. the same inside the build directory `target`: Done;
/// 3. commits a file under `docs/frozen`: Needs you (`guard_protected_path`), the verdict never
///    runs (`mutant_git_skip_protected_paths` and `mutant_loop_skip_guards` let it reach Done);
/// 4. the same with `allow-protected = ["docs/frozen"]` in its contract: Done;
/// 5. plants a hook in `.git/hooks`: Needs you (`guard_git_config`)
///    (`mutant_git_skip_config_snapshot` lets it reach Done);
/// 6. commits twice, the second time with the trailer marker turned off: Done. The first
///    commit carries `Item:`, `Run:` and `Session:` equal to the attempt's and the report lists
///    no `trailer_missing` for it; the second carries none and is the report's one
///    `trailer_missing` line (`mutant_git_trailer_all_commits` puts the trailers on it too).
///
/// Each attempt's guard snapshots are stored on its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn the_git_guards_refuse_before_the_verdict_and_the_trailers_reach_the_report() {
    let scene = Scene::new().await;
    std::fs::write(scene.repo.join(".gitignore"), "*.log\n").unwrap();
    scene.git(&["add", ".gitignore"]);
    scene.git(&["commit", "-q", "-m", "Ignore logs"]);
    scene.write_board(&[
        card(1, "Hidden", "test -f hidden.log", "Item one."),
        card(
            2,
            "Build output",
            "test -f target/out.log && test -f two.txt",
            "Item two.",
        ),
        card(3, "Protected", "test -f docs/frozen/a.md", "Item three."),
        card_allowing(4, "Allowed", "test -f docs/frozen/b.md", "docs/frozen"),
        card(5, "Planted hook", "test -f five.txt", "Item five."),
        card(6, "Trailers", "test -f six-b.txt", "Item six."),
    ]);
    scene.script(
        1,
        &[
            "write hidden.log x".to_owned(),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            "write target/out.log x".to_owned(),
            "commit two.txt two".to_owned(),
        ],
    );
    scene.script(3, &["commit docs/frozen/a.md a".to_owned()]);
    scene.script(4, &["commit docs/frozen/b.md b".to_owned()]);
    scene.script(
        5,
        &[
            "write .git/hooks/pre-push x".to_owned(),
            "commit five.txt five".to_owned(),
        ],
    );
    scene.script(
        6,
        &[
            "commit six.txt six".to_owned(),
            format!("unset {}", bosun_git::MARKER_VAR),
            "commit six-b.txt six-b".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let mut states = BTreeMap::new();
    for n in 1..=6 {
        states.insert(n, ended(&scene, n, Duration::from_secs(120)).await);
    }
    task.abort();
    watcher.abort();

    let guard = |g| State::NeedsYou(Reason::Guard(g));
    assert_eq!(states[&1], guard(bosun_core::Guard::IgnoredFile), "item 1");
    assert!(
        matches!(states[&2], State::Done(_)),
        "item 2: {:?}",
        states[&2]
    );
    assert_eq!(
        states[&3],
        guard(bosun_core::Guard::ProtectedPath),
        "item 3"
    );
    assert!(
        matches!(states[&4], State::Done(_)),
        "item 4: {:?}",
        states[&4]
    );
    assert_eq!(states[&5], guard(bosun_core::Guard::GitConfig), "item 5");
    assert!(
        matches!(states[&6], State::Done(_)),
        "item 6: {:?}",
        states[&6]
    );

    let id = |n: u64| ItemId::new(format!("demo#{n}"));
    let reader = scene.store.reader();
    // Refused before the verdict: no verdict ran for the refused items.
    for n in [1, 3, 5] {
        assert_eq!(
            reader.verdict_runs(&id(n)).await.unwrap().len(),
            0,
            "item {n}"
        );
    }
    // Nothing is deleted: the hidden file is still there.
    assert!(scene.repo.join("hidden.log").exists());
    // The guard snapshots are stored with the attempt: item 3's lists what items 1 and 2 left.
    let (ignored, git_config) = reader.attempt_guards(&id(3), 1).await.unwrap().unwrap();
    let ignored: Vec<String> = serde_json::from_str(&ignored).unwrap();
    assert_eq!(ignored, ["hidden.log", "target/out.log"]);
    let git_config: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&git_config).unwrap();
    assert!(
        git_config.keys().any(|k| k.ends_with("config")),
        "{git_config:?}"
    );

    // Item 6's commits: the first with the attempt's trailers, the second with none.
    let runs = reader.runs(&id(6)).await.unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let session = runs[0].session_key.clone();
    assert_eq!(trailer(&scene, "HEAD~1", "Item"), ["demo#6"]);
    assert_eq!(trailer(&scene, "HEAD~1", "Run"), ["a1s1"]);
    assert_eq!(trailer(&scene, "HEAD~1", "Session"), [session]);
    for key in ["Item", "Run", "Session"] {
        assert_eq!(trailer(&scene, "HEAD", key), Vec::<String>::new(), "{key}");
    }

    // The report: the commits of item 6 linked to its run, the one without trailers listed.
    let repo_key = scene.config().repos["demo"].key.clone();
    bosun_ingest::git::refresh(reader, scene.store.writer(), &scene.repo)
        .await
        .unwrap();
    let report = bosun_heat::exact(
        reader,
        "test",
        bosun_heat::Scope {
            from_ms: 0,
            to_ms: now_ms() + 60_000,
            repo: Some(&repo_key),
        },
        |key| bosun_api::RepoRef {
            name: Some("demo".to_owned()),
            path: key.to_owned(),
        },
        Vec::new(),
    )
    .await
    .unwrap();
    let head = scene.git(&["rev-parse", "HEAD"]);
    let first = scene.git(&["rev-parse", "HEAD~1"]);
    let linked: Vec<&str> = report
        .items
        .commits
        .iter()
        .filter(|c| c.item == "demo#6")
        .map(|c| c.sha.as_str())
        .collect();
    assert!(
        linked.contains(&first.as_str()) && linked.contains(&head.as_str()),
        "{linked:?}"
    );
    let missing: Vec<(&str, &str)> = report
        .items
        .trailer_missing
        .iter()
        .map(|t| (t.item.as_str(), t.sha.as_str()))
        .collect();
    assert_eq!(missing, [("demo#6", head.as_str())]);
}

/// #137: an attempt refused by a precondition keeps its reasons. The agent commits, then leaves
/// `loose.txt` untracked, twice: both attempts fail the clean tree, and the item ends Needs you
/// (`attempts_exhausted`). The reasons are on each attempt's row, in the item report, and in
/// the second attempt's prompt as the daemon's words
/// (`mutant_executor_drop_precondition_reasons` stores none: all three lose them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_precondition_refusal_reaches_the_report_and_the_next_prompt() {
    let scene = Scene::new().await;
    let prompt = scene.path("prompt");
    scene.write_board(&[card(1, "Loose", "test -f loose.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("stdin {}", prompt.display()),
            "git commit --allow-empty -q -m one".to_owned(),
            "write loose.txt x".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::AttemptsExhausted));

    let id = ItemId::new("demo#1");
    let reader = scene.store.reader();
    let attempts = reader.attempts(&id).await.unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    for attempt in &attempts {
        assert!(
            attempt
                .refusal
                .iter()
                .any(|r| r.contains("the working tree is not clean") && r.contains("loose.txt")),
            "attempt {}: {:?}",
            attempt.attempt.number,
            attempt.refusal
        );
    }

    let facts = super::outbox::report_facts(reader, &id, "Loose".to_owned(), "demo", None)
        .await
        .unwrap();
    let report = bosun_core::publish::render_report(
        &facts,
        bosun_core::publish::ReportOptions::for_repository(false),
    );
    assert!(
        report.contains(
            "- Attempt 1: fail, refused before its verdict: the working tree is not clean"
        ),
        "{report}"
    );

    // The second attempt's prompt (the script copies each prompt to the same file).
    let prompt = std::fs::read_to_string(&prompt).unwrap();
    assert!(prompt.contains("This is attempt 2 of 2."), "{prompt}");
    assert!(
        prompt.contains("Your previous attempt was refused by the daemon before its verdict ran"),
        "{prompt}"
    );
    assert!(
        prompt.contains("- the working tree is not clean: ?? loose.txt"),
        "{prompt}"
    );
}

/// #99: a second daemon on the same board. Card 1 carries the claim marker of another daemon id
/// (of the same identity, so its login wrote it): the repository is refused, card 1 is not
/// claimed (nothing written to the board, no snapshot, no session) and neither is card 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_on_the_board_is_refused_and_claims_nothing() {
    let scene = Scene::on_projects().await;
    let fake = scene.projects.clone().unwrap();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    fake.issue(BOARD_REPO, 1, |issue| {
        issue.comments.push(FakeComment::by(
            LOGIN,
            &bosun_queue::marker::embed(
                &bosun_queue::claim::claim_key("other-host/other-user"),
                "bosun: claimed by the daemon `other-host/other-user`.",
            ),
        ));
    });
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    // Long enough for many polls: the refusal holds every claim.
    tokio::time::sleep(Duration::from_secs(3)).await;
    task.abort();
    watcher.abort();

    assert_eq!(fake.count("UpdateStatus"), 0, "{:?}", fake.requests());
    assert_eq!(fake.count("AddComment"), 0, "{:?}", fake.requests());
    for n in [1, 2] {
        let id = ItemId::new(format!("demo#{n}"));
        let state = scene.state(n).await;
        assert!(
            matches!(state, None | Some(State::Ready)),
            "card {n}: {state:?}"
        );
        assert!(scene.store.reader().snapshot(&id).await.unwrap().is_none());
        assert_eq!(fake.status_of(0, BOARD_REPO, n).as_deref(), Some("Ready"));
    }
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
}

/// #99: a board that does not match the configuration (no `Needs you` column) fails the
/// connection at the loop's first poll with a setup error: the repository is paused, claims
/// nothing, and is not asked again within the pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_board_that_does_not_match_pauses_the_repository() {
    let scene = Scene::on_projects().await;
    let fake = scene.projects.clone().unwrap();
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    fake.set_options(0, &["Backlog", "Ready", "In progress", "Verify", "Done"]);
    let (task, watcher) = scene.start(None);
    tokio::time::sleep(Duration::from_secs(2)).await;
    task.abort();
    watcher.abort();
    assert_eq!(fake.count("Resolve"), 1, "{:?}", fake.requests());
    assert_eq!(fake.count("Items"), 0, "{:?}", fake.requests());
    assert_eq!(scene.state(1).await, None);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
}

/// #99: the identity checks pass before the claim and refuse at the session (the token's
/// login changed in between, here during the verdict on the base commit): Needs you as
/// `identity`, nothing spawned, no attempt failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identity_refused_at_the_session_is_needs_you_as_identity() {
    let scene = Scene::new().await;
    let login = sh_path(&scene.path("login"));
    scene.write_board(&[card(
        1,
        "Login changes",
        &format!("echo someone-else > {login}; test -f one.txt"),
        "Item one.",
    )]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::Identity));
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.failed, 0, "{item:?}");
}

/// #133: a card without `model` asks for the repository's default (`model = 'opus'` in the
/// scene's configuration); a card's `model` overrides it. The item report names the model
/// asked for. `mutant_runner_no_default_model` leaves the repository's default out, and the
/// first session gets no `--model`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_asks_for_the_card_model_else_the_repository_default() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    let mut haiku = card(2, "Overrides", "test -f two.txt", "Item two.");
    haiku.body = haiku
        .body
        .replace("verdict = ", "model = \"haiku\"\nverdict = ");
    scene.write_board(&[card(1, "Defaults", "test -f one.txt", "Item one."), haiku]);
    for (n, file) in [(1, "one.txt"), (2, "two.txt")] {
        scene.script(
            n,
            &[
                format!("argv {}", p(&format!("argv{n}"))),
                format!("commit {file} x"),
            ],
        );
    }
    let (task, watcher) = scene.start(None);
    for n in [1, 2] {
        let state = ended(&scene, n, Duration::from_secs(60)).await;
        assert_eq!(state, State::Done(Outcome::Pass), "item {n}");
    }
    drained(&scene).await;
    task.abort();
    watcher.abort();

    let model_of = |n: u64| {
        let argv = std::fs::read_to_string(scene.path(&format!("argv{n}"))).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        argv.iter()
            .position(|a| *a == "--model")
            .map(|i| argv[i + 1].to_owned())
    };
    assert_eq!(
        model_of(1).as_deref(),
        Some("opus"),
        "the repository's default"
    );
    assert_eq!(model_of(2).as_deref(), Some("haiku"), "the card's override");

    let cards = scene.cards();
    for (n, want) in [(1, "opus"), (2, "haiku")] {
        let card = cards.iter().find(|c| c.id.0 == n).unwrap();
        let report = card
            .comments
            .iter()
            .find(|c| c.key.as_deref() == Some(&*format!("report:demo#{n}")))
            .map(|c| c.body.clone())
            .expect("the sticky report");
        let want = format!("- Model asked: `{want}`");
        assert!(report.contains(&want), "{want:?} missing from {report}");
    }
}

#[path = "control_tests.rs"]
mod control;

#[path = "board_tests.rs"]
mod board_scenes;
