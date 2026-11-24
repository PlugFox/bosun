use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

struct Local;

impl DiskProbe for Local {
    fn probe(&self, _: &Path) -> Disk {
        Disk::Local
    }
}

fn abs(rest: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\{}", rest.replace('/', "\\"))
    } else {
        format!("/{rest}")
    }
}

fn problems(text: &str) -> Vec<String> {
    match parse(text, &Local) {
        Ok(config) => panic!("accepted: {config:?}"),
        Err(Error::Invalid(problems)) => problems.iter().map(ToString::to_string).collect(),
        Err(e) => panic!("{e}"),
    }
}

/// One identity and one repository, with `rest` appended to the repository's table.
fn with_repo(rest: &str) -> String {
    format!(
        "[identity.personal]\n\
         claude_config_dir = '{}'\n\
         database = '{}'\n\
         [repo.bosun]\n\
         path = '{}'\n\
         identity = 'personal'\n\
         {rest}",
        abs("home/.claude"),
        abs("data/personal.db"),
        abs("git/bosun"),
    )
}

#[test]
fn a_malformed_file_is_refused_with_its_line_and_column() {
    let found = problems("[daemon]\n\n[identity.personal]\ndatabase = 'unterminated\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("line 4, column "), "{found:?}");

    let found = problems("[repo.a]\npath = 'x'\npath = 'y'\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("line 3, column 1:"), "{found:?}");
}

#[test]
fn the_stage_0_subset_is_read() {
    let text = format!(
        "[daemon]\n\
         [identity.personal]\n\
         claude_config_dir = '{}'\n\
         database = '{}'\n\
         [repo.bosun]\n\
         path = '{}'\n\
         identity = 'personal'\n\
         [retention]\n\
         reingest_window = \"2w\"\n",
        abs("home/.claude"),
        abs("data/personal.db"),
        abs("git/bosun"),
    );
    let config = parse(&text, &Local).unwrap();
    assert_eq!(config.notices, []);
    let personal = &config.identities["personal"];
    assert_eq!(
        personal.claude_config_dir,
        PathBuf::from(abs("home/.claude"))
    );
    assert_eq!(personal.database, PathBuf::from(abs("data/personal.db")));
    assert_eq!(config.repos["bosun"].identity, "personal");
    assert_eq!(
        config.retention.reingest_window,
        Duration::from_hours(14 * 24)
    );

    // Every stage-1 key has its default.
    assert_eq!(personal.github_host, DEFAULT_GITHUB_HOST);
    assert_eq!(personal.github_login, None);
    assert_eq!(personal.gh_config_dir, None);
    assert!(personal.remote_allowlist.is_empty());
    let repo = &config.repos["bosun"];
    assert_eq!(repo.queue, None);
    assert_eq!(repo.verdict, Verdict::default());
    assert_eq!(repo.policy, Policy::default());
    assert!(!repo.policy.push);
    assert!(repo.build_dirs.is_empty());
    assert_eq!(repo.landing.mode, LandingMode::Local);
    assert!(repo.mcp_servers.is_empty());
    assert_eq!(config.budget, Budget::default());
    assert_eq!(config.daemon, Daemon::default());

    let empty = parse("", &Local).unwrap();
    assert!(empty.identities.is_empty() && empty.repos.is_empty());
    assert_eq!(
        empty.retention.reingest_window,
        read::DEFAULT_REINGEST_WINDOW
    );
}

#[test]
#[allow(clippy::too_many_lines)] // one document, every stage-1 key
fn the_stage_1_keys_are_read() {
    let text = format!(
        "[daemon]\n\
         auto_pause = false\n\
         auto_pause_idle = '45m'\n\
         question_grace = '1m'\n\
         kill_grace = '5s'\n\
         hello_timeout = '20s'\n\
         park_cap = 3\n\
         mcp_call_cap = 10\n\
         [identity.personal]\n\
         claude_config_dir = '{}'\n\
         database = '{}'\n\
         github_host = 'github.example'\n\
         github_login = 'someone'\n\
         gh_config_dir = '{}'\n\
         git_config = '{}'\n\
         ssh_key = '{}'\n\
         codex_home = '{}'\n\
         remote_allowlist = ['someone', 'an-org', 'someone']\n\
         author_allowlist = ['someone']\n\
         [repo.bosun]\n\
         path = '{}'\n\
         identity = 'personal'\n\
         build_dirs = ['target/', 'out']\n\
         queue = {{ kind = 'projects', owner = 'someone', number = 7 }}\n\
         model = 'opus'\n\
         verdict = {{ timeout = '10m', shell = 'bash' }}\n\
         landing = {{ mode = 'local' }}\n\
         [repo.bosun.policy]\n\
         protected = ['tests/harness/', 'xtask/src/lint.rs']\n\
         push = true\n\
         [repo.bosun.mcp_servers.docs]\n\
         command = 'docs-server'\n\
         args = ['--stdio', '-v']\n\
         env = {{ LEVEL = 'info' }}\n\
         [repo.other]\n\
         path = '{}'\n\
         identity = 'personal'\n\
         queue = {{ kind = 'local_file', file = '{}' }}\n\
         model = 'claude-sonnet-4-5'\n\
         [budget]\n\
         item_eqv = 5_000_000\n\
         item_wall_clock = '2h'\n\
         daily_eqv = 40_000_000\n\
         reserve_eqv = 5_000_000\n",
        abs("home/.claude"),
        abs("data/personal.db"),
        abs("home/gh"),
        abs("home/gitconfig"),
        abs("home/ssh/id"),
        abs("home/codex"),
        abs("git/bosun"),
        abs("git/other"),
        abs("git/other/cards.toml"),
    );
    let config = parse(&text, &Local).unwrap();
    assert_eq!(config.notices, []);

    assert_eq!(
        config.daemon,
        Daemon {
            auto_pause: false,
            auto_pause_idle: Duration::from_mins(45),
            question_grace: Duration::from_mins(1),
            kill_grace: Duration::from_secs(5),
            hello_timeout: Duration::from_secs(20),
            park_cap: 3,
            mcp_call_cap: 10,
        }
    );

    let personal = &config.identities["personal"];
    assert_eq!(personal.github_host, "github.example");
    assert_eq!(personal.github_login.as_deref(), Some("someone"));
    assert_eq!(personal.gh_config_dir, Some(PathBuf::from(abs("home/gh"))));
    assert_eq!(
        personal.git_config,
        Some(PathBuf::from(abs("home/gitconfig")))
    );
    assert_eq!(personal.ssh_key, Some(PathBuf::from(abs("home/ssh/id"))));
    assert_eq!(personal.codex_home, Some(PathBuf::from(abs("home/codex"))));
    assert_eq!(
        personal.remote_allowlist,
        BTreeSet::from(["an-org".to_owned(), "someone".to_owned()])
    );
    assert_eq!(
        personal.author_allowlist,
        BTreeSet::from(["someone".to_owned()])
    );

    let repo = &config.repos["bosun"];
    assert_eq!(
        repo.queue,
        Some(Queue::Projects {
            owner: "someone".to_owned(),
            number: 7
        })
    );
    assert_eq!(repo.model.as_deref(), Some("opus"));
    assert_eq!(repo.verdict.timeout, Duration::from_mins(10));
    assert_eq!(repo.verdict.shell.as_deref(), Some("bash"));
    assert_eq!(
        repo.build_dirs,
        BTreeSet::from(["out".to_owned(), "target".to_owned()])
    );
    assert_eq!(
        repo.policy.protected,
        BTreeSet::from([
            ".gitignore".to_owned(),
            "tests/harness".to_owned(),
            "xtask/src/lint.rs".to_owned(),
        ])
    );
    assert!(repo.policy.push);
    assert!(repo.policy.protects("tests/harness/run.rs"));
    let docs = &repo.mcp_servers["docs"];
    assert_eq!(docs.command, "docs-server");
    assert_eq!(docs.args, ["--stdio", "-v"]);
    assert_eq!(docs.env["LEVEL"], "info");

    assert_eq!(
        config.repos["other"].queue,
        Some(Queue::LocalFile {
            file: PathBuf::from(abs("git/other/cards.toml"))
        })
    );

    assert_eq!(
        config.budget,
        Budget {
            item_eqv: Some(5_000_000),
            item_wall_clock: Some(Duration::from_hours(2)),
            daily_eqv: Some(40_000_000),
            reserve_eqv: 5_000_000,
        }
    );
}

#[test]
fn later_sections_and_keys_are_ignored_with_a_notice() {
    let text = format!(
        "[daemon]\nhttp_port = 8080\n\
         [identity.personal]\n\
         claude_config_dir = '{}'\n\
         database = '{}'\n\
         runners = []\n\
         [repo.bosun]\npath = '{}'\nidentity = 'personal'\n\
         report = {{ cost = true }}\n\
         landing = {{ mode = 'local', target = 'main' }}\n\
         [budget]\ndaily = 1\n\
         [notify.telegram]\nchat = 1\n",
        abs("home/.claude"),
        abs("data/personal.db"),
        abs("git/bosun"),
    );
    let config = parse(&text, &Local).unwrap();
    let notices: Vec<String> = config.notices.iter().map(ToString::to_string).collect();
    assert_eq!(
        notices,
        [
            "line 2, column 1: `daemon.http_port` is not read in this version; ignored",
            "line 6, column 1: `identity.personal.runners` is not read in this version; ignored",
            "line 10, column 1: `repo.bosun.report` is not read in this version; ignored",
            "line 11, column 29: `repo.bosun.landing.target` is not read in this version; \
             ignored",
            "line 13, column 1: `budget.daily` is not read in this version; ignored",
            "line 14, column 2: section `notify` is not read in this version; ignored",
        ]
    );
}

#[test]
fn shapes_are_checked() {
    let found = problems(
        "[identity.personal]\nclaude_config_dir = 'relative/dir'\n\
         [repo.x]\npath = 1\n\
         [retention]\nreingest_window = '30 days'\n\
         [unknown]\n",
    );
    assert_eq!(found.len(), 6, "{found:?}");
    assert!(
        found[0].contains("`identity.personal` has no `database`"),
        "{found:?}"
    );
    assert!(found[1].contains("must be absolute"), "{found:?}");
    assert!(found[2].contains("`repo.x` has no `identity`"), "{found:?}");
    assert!(
        found[3].contains("`repo.x.path` must be a string"),
        "{found:?}"
    );
    assert!(
        found[4].contains("expected a positive whole number"),
        "{found:?}"
    );
    assert!(found[5].contains("unknown section `unknown`"), "{found:?}");
}

#[test]
fn stage_1_shapes_are_checked() {
    let found = problems(&with_repo(
        "build_dirs = ['C:/abs', 'a\\b']\n\
         queue = { kind = 'jira' }\n\
         model = 'opus'\n\
         verdict = { timeout = '0s' }\n\
         [repo.bosun.policy]\n\
         protect = ['tests']\n\
         protected = ['src/*.rs']\n\
         push = 'no'\n\
         [repo.bosun.mcp_servers.bosun]\n\
         command = 'x'\n\
         [repo.bosun.mcp_servers.docs]\n\
         args = []\n\
         [daemon]\n\
         park_cap = 0\n\
         [budget]\n\
         daily_eqv = 10\n\
         reserve_eqv = 10\n",
    ));
    let expected = [
        "`repo.bosun.build_dirs` holds \"C:/abs\": it must be relative",
        "`repo.bosun.build_dirs` holds \"a\\\\b\": use `/`",
        "`repo.bosun.queue.kind` is \"jira\"",
        "`repo.bosun.verdict.timeout` is \"0s\"",
        "unknown key `repo.bosun.policy.protect`",
        "`repo.bosun.policy.protected` holds \"src/*.rs\": wildcards are not supported",
        "`repo.bosun.policy.push` must be true or false",
        "the name `bosun` is the daemon's own server",
        "`repo.bosun.mcp_servers.docs` has no `command`",
        "`daemon.park_cap` must be a positive whole number",
        "`budget.reserve_eqv` is 10, not less than `budget.daily_eqv` (10)",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for (found, expected) in found.iter().zip(expected) {
        assert!(found.contains(expected), "{found:?} lacks {expected:?}");
        assert!(found.starts_with("line "), "{found:?} has no location");
    }
}

#[test]
fn identities_never_share_a_bundle_path() {
    let identity = |name: &str, gh: &str| {
        format!(
            "[identity.{name}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\
             gh_config_dir = '{}'\n",
            abs(&format!("home/{name}/.claude")),
            abs(&format!("data/{name}.db")),
            abs(gh),
        )
    };
    let found = problems(&(identity("personal", "home/gh") + &identity("work", "home/gh")));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains(
            "`identity.personal` and `identity.work` share the gh configuration directory"
        ),
        "{found:?}"
    );
    parse(
        &(identity("personal", "home/gh-1") + &identity("work", "home/gh-2")),
        &Local,
    )
    .unwrap();
}

#[test]
fn a_landing_mode_other_than_local_is_refused_naming_8() {
    for mode in ["pr", "merge", "push"] {
        let found = problems(&with_repo(&format!("landing = {{ mode = '{mode}' }}\n")));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("line 7, column 20: `repo.bosun.landing.mode`"),
            "{found:?}"
        );
        assert!(found[0].contains("#8"), "{found:?}");
    }
}

/// #133: a repository with a queue and no `model` is refused at load, the error naming the
/// key; an ingest-only repository needs none; a model must be a non-empty word.
/// `mutant_config_queue_without_model` accepts the missing model.
#[test]
fn a_repository_with_a_queue_and_no_model_is_refused() {
    let queue = "queue = { kind = 'projects', owner = 'someone', number = 7 }\n";
    let found = problems(&with_repo(queue));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`repo.bosun` has a `queue` and no `model`"),
        "{found:?}"
    );
    assert!(found[0].starts_with("line "), "{found:?} has no location");

    let config = parse(&with_repo(&format!("{queue}model = 'opus'\n")), &Local).unwrap();
    assert_eq!(config.repos["bosun"].model.as_deref(), Some("opus"));
    let ingest_only = parse(&with_repo(""), &Local).unwrap();
    assert_eq!(ingest_only.repos["bosun"].model, None);

    let found = problems(&with_repo(&format!("{queue}model = ' '\n")));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`repo.bosun.model` is \" \""),
        "{found:?}"
    );
}

#[test]
fn a_policy_that_omits_gitignore_still_protects_it() {
    for rest in [
        "",
        "[repo.bosun.policy]\n",
        "[repo.bosun.policy]\nprotected = ['tests']\n",
    ] {
        let config = parse(&with_repo(rest), &Local).unwrap();
        let policy = &config.repos["bosun"].policy;
        assert!(policy.protects(".gitignore"), "{rest:?}: {policy:?}");
        assert!(policy.protects("crates/a/.gitignore"), "{rest:?}");
        assert!(!policy.protects("src/main.rs"), "{rest:?}");
    }
    // Listing it explicitly is the same policy as leaving it out.
    let implicit = parse(
        &with_repo("[repo.bosun.policy]\nprotected = ['tests']\n"),
        &Local,
    )
    .unwrap();
    let explicit = parse(
        &with_repo("[repo.bosun.policy]\nprotected = ['.gitignore', 'tests']\n"),
        &Local,
    )
    .unwrap();
    assert_eq!(
        implicit.repos["bosun"].policy,
        explicit.repos["bosun"].policy
    );
}

#[test]
fn the_policy_hash_follows_protected_paths_only() {
    let base = with_repo(
        "# the freeze list\n\
         build_dirs = ['target']\n\
         [repo.bosun.policy]\n\
         protected = ['tests/harness', 'xtask/src/lint.rs']\n\
         push = false\n",
    );
    // The same values: keys and entries reordered, comments and spacing edited.
    let reordered = format!(
        "[repo.bosun]  # moved first\n\
         identity = 'personal'\n\
         path = '{}'\n\
         build_dirs = [ 'target', ]\n\
         [repo.bosun.policy]\n\
         push = false # still denied\n\
         protected = [\n  'xtask/src/lint.rs',\n  'tests/harness',  # a comment\n]\n\
         [identity.personal]\n\
         database = '{}'\n\
         claude_config_dir = '{}'\n",
        abs("git/bosun"),
        abs("data/personal.db"),
        abs("home/.claude"),
    );
    let changed = base.replace("tests/harness", "tests/harness2");
    let push = base.replace("push = false", "push = true");

    let hashes = |text: &str| {
        let config = parse(text, &Local).unwrap();
        (
            config.repos["bosun"].policy.hash(),
            config.repo_hash("bosun").unwrap(),
        )
    };
    let (base_policy, base_repo) = hashes(&base);
    assert_eq!(base_policy.len(), 64);
    assert!(base_policy.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(hashes(&reordered), (base_policy.clone(), base_repo.clone()));

    let (changed_policy, changed_repo) = hashes(&changed);
    assert_ne!(changed_policy, base_policy, "a protected path changed");
    assert_ne!(changed_repo, base_repo, "a protected path changed");

    let (push_policy, push_repo) = hashes(&push);
    assert_ne!(push_policy, base_policy, "push changed");
    assert_ne!(push_repo, base_repo, "push changed");
}

#[test]
fn the_repository_hash_covers_what_governs_its_items() {
    let base = with_repo("")
        + &format!(
            "[repo.other]\npath = '{}'\nidentity = 'personal'\n",
            abs("git/other")
        );
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let before = hash(&base);

    // Its own keys, its identity, the budget and the daemon section change it.
    for edit in [
        base.replace("[repo.other]", "verdict = { timeout = '5m' }\n[repo.other]"),
        base.replace(
            "identity = 'personal'\n[repo.other]",
            "identity = 'personal'\nqueue = { kind = 'projects', owner = 'o', number = 1 }\n\
             model = 'opus'\n[repo.other]",
        ),
        base.replace(
            "identity = 'personal'\n[repo.other]",
            "identity = 'personal'\nmodel = 'haiku'\n[repo.other]",
        ),
        base.replace("[repo.bosun]", "github_login = 'x'\n[repo.bosun]"),
        base.clone() + "[budget]\nitem_eqv = 1\n",
        base.clone() + "[daemon]\npark_cap = 1\n",
    ] {
        assert_ne!(hash(&edit), before, "{edit}");
    }
    // Another repository, retention and a later section do not.
    for edit in [
        base.clone() + "build_dirs = ['target']\n",
        base.clone() + "[retention]\nreingest_window = '1w'\n",
        base.clone() + "[notify.x]\n",
    ] {
        assert_eq!(hash(&edit), before, "{edit}");
    }
    assert_eq!(parse(&base, &Local).unwrap().repo_hash("missing"), None);
}

#[test]
fn a_broken_edit_keeps_the_previous_configuration() {
    let good = with_repo("[repo.bosun.policy]\nprotected = ['tests']\n");
    let previous = parse(&good, &Local).unwrap();

    // Malformed TOML, and well-formed TOML that fails validation.
    for (broken, at) in [
        (good.replace("['tests']", "['tests'"), "line 8, column "),
        (
            good.replace("identity = 'personal'", "identity = 'nobody'"),
            "line 6, column 12:",
        ),
        (
            with_repo("landing = { mode = 'pr' }\n"),
            "line 7, column 20:",
        ),
    ] {
        let reloaded = reload(previous.clone(), &broken, &Local);
        assert_eq!(reloaded.config, previous, "{broken}");
        let Some(Error::Invalid(problems)) = reloaded.error else {
            panic!("no error for {broken}");
        };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].to_string().starts_with(at), "{problems:?}");
    }

    // A valid edit is adopted.
    let edited = good.replace("['tests']", "['tests', 'gate.sh']");
    let reloaded = reload(previous.clone(), &edited, &Local);
    assert!(reloaded.error.is_none());
    assert_ne!(reloaded.config, previous);
    assert!(reloaded.config.repos["bosun"].policy.protects("gate.sh"));
}

#[test]
fn load_reports_an_unreadable_file() {
    let missing = std::env::temp_dir().join("bosun-config-test-missing.toml");
    assert!(matches!(load(&missing, &Local), Err(Error::Read { .. })));
}

#[test]
fn the_default_location_follows_the_platform() {
    let root = PathBuf::from(abs("profile"));
    let env = |name: &str| -> Option<OsString> {
        match name {
            "LOCALAPPDATA" | "HOME" => Some(root.clone().into()),
            _ => None,
        }
    };
    let expected = if cfg!(windows) {
        root.join("bosun")
    } else if cfg!(target_os = "macos") {
        root.join("Library")
            .join("Application Support")
            .join("bosun")
    } else {
        root.join(".config").join("bosun")
    };
    assert_eq!(default_path_in(&env), Some(expected.join("config.toml")));

    let explicit = PathBuf::from(abs("elsewhere/bosun.toml"));
    let env = |name: &str| (name == "BOSUN_CONFIG").then(|| explicit.clone().into());
    assert_eq!(default_path_in(&env), Some(explicit.clone()));

    assert_eq!(default_path_in(&|_| None), None);
}

#[test]
fn a_resource_needs_a_positive_capacity_and_a_known_scope() {
    let found = problems(
        "[resource.target]\n\
         kind = 'build-dir'\n\
         capacity = 0\n\
         [resource.gpu]\n\
         kind = 'gpu'\n\
         scope = 'planet'\n\
         [resource.device]\n\
         capacity = 2\n\
         sharing = 'all'\n",
    );
    let expected = [
        "line 3, column 12: `resource.target.capacity` must be a positive whole number",
        "line 6, column 9: `resource.gpu.scope` is \"planet\"; it is \"machine\", \"identity\" \
         or \"repository\"",
        "line 7, column 11: `resource.device` has no `kind`",
        "line 9, column 1: unknown key `resource.device.sharing`",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for (found, expected) in found.iter().zip(expected) {
        assert!(found.starts_with(expected), "{found:?} is not {expected:?}");
    }

    let config = parse(
        "[resource.target]\n\
         kind = 'build-dir'\n\
         [resource.gpu]\n\
         kind = 'gpu'\n\
         capacity = 2\n\
         scope = 'identity'\n",
        &Local,
    )
    .unwrap();
    assert_eq!(config.notices, []);
    let target = &config.resources["target"];
    assert_eq!(target.kind, "build-dir");
    assert_eq!(target.capacity, 1);
    assert_eq!(target.scope, Scope::Machine);
    let gpu = &config.resources["gpu"];
    assert_eq!((gpu.capacity, gpu.scope), (2, Scope::Identity));
    assert!(parse("", &Local).unwrap().resources.is_empty());
}

#[test]
fn a_worktree_root_inside_the_repository_is_refused() {
    // The repository is `git/bosun`; on Windows the root is matched whatever its case.
    let inside = if cfg!(windows) {
        "GIT/Bosun/.worktrees/"
    } else {
        "git/bosun/.worktrees/"
    };
    for root in [inside, "git/bosun"] {
        let found = problems(&with_repo(&format!(
            "worktree = {{ root = '{}' }}\n",
            abs(root)
        )));
        assert_eq!(found.len(), 1, "{root}: {found:?}");
        assert!(
            found[0].starts_with("line 7, column 21: `repo.bosun.worktree.root`")
                && found[0].contains("is inside the repository's path"),
            "{found:?}"
        );
    }

    // A sibling directory, even one whose name starts with the repository's, is outside.
    for root in ["git/bosun-worktrees", "wt/bosun"] {
        let text = with_repo(&format!(
            "worktree = {{ root = '{}', setup = 'cargo fetch', teardown = 'cargo clean', \
             disk_floor = '20GB' }}\n",
            abs(root)
        ));
        let config = parse(&text, &Local).unwrap();
        assert_eq!(config.notices, []);
        assert_eq!(
            config.repos["bosun"].worktree,
            Some(Worktree {
                root: PathBuf::from(abs(root)),
                setup: Some("cargo fetch".to_owned()),
                teardown: Some("cargo clean".to_owned()),
                disk_floor: Some(20_000_000_000),
            })
        );
    }
    assert_eq!(
        parse(&with_repo(""), &Local).unwrap().repos["bosun"].worktree,
        None
    );

    // The worktree's other keys are checked too, each named.
    let found = problems(&with_repo(&format!(
        "worktree = {{ root = '{}', disk_floor = '20', rot = 'x' }}\n",
        abs("wt/bosun")
    )));
    let expected = [
        "`repo.bosun.worktree.disk_floor` is \"20\"",
        "unknown key `repo.bosun.worktree.rot`",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for (found, expected) in found.iter().zip(expected) {
        assert!(found.contains(expected), "{found:?} lacks {expected:?}");
    }
    let found = problems(&with_repo("worktree = { setup = 'x' }\n"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`repo.bosun.worktree` has no `root`"),
        "{found:?}"
    );
}
