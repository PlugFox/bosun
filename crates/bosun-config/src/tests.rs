use std::collections::{BTreeMap, BTreeSet};
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
         fast_death_sessions = 4\n\
         fast_death_within = '90s'\n\
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
            fast_death_sessions: 4,
            fast_death_within: Duration::from_secs(90),
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
         landing = {{ mode = 'local', pr = {{ title = 'x' }} }}\n\
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
            "line 11, column 36: `repo.bosun.landing.pr.title` is not read in this version; \
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
fn landing_shapes_are_checked() {
    let found = problems(&with_repo(
        "landing = { mode = 'push', target = 'refs/heads/main', merge = { method = 'rebase', \
         when = 'now' }, pr = { draft = 'yes', reviewers = [' x'], labels = [''], auto = 1 } }
",
    ));
    let expected = [
        "`repo.bosun.landing.mode` is \"push\"; it is \"local\", \"merge\" or \"pr\"",
        "`repo.bosun.landing.target` is \"refs/heads/main\"; write the bare branch name",
        "`repo.bosun.landing.merge.method` is \"rebase\"",
        "unknown key `repo.bosun.landing.merge.when`",
        "`repo.bosun.landing.pr.draft` must be true or false",
        "`repo.bosun.landing.pr.reviewers` holds \" x\"",
        "`repo.bosun.landing.pr.labels` holds \"\"",
        "unknown key `repo.bosun.landing.pr.auto`",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for (found, expected) in found.iter().zip(expected) {
        assert!(found.contains(expected), "{found:?} lacks {expected:?}");
        assert!(found.starts_with("line 7, "), "{found:?} has no location");
    }
    for target in ["", "a b", "-x"] {
        let found = problems(&with_repo(&format!(
            "landing = {{ target = '{target}' }}
"
        )));
        assert_eq!(found.len(), 1, "{target:?}: {found:?}");
        assert!(
            found[0].contains("`repo.bosun.landing.target` is"),
            "{found:?}"
        );
    }
    // `merge` is read with `mode = "merge"` only.
    let found = problems(&with_repo(
        "landing = { mode = 'local', merge = { method = 'squash' } }
",
    ));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`repo.bosun.landing.merge` is set with the mode \"local\""),
        "{found:?}"
    );
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
            with_repo("landing = { mode = 'merge' }\n"),
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
fn a_resource_artifact_needs_a_local_path_a_device_path_and_a_digest_command() {
    let config = parse(
        "[resource.rig]\n\
         kind = 'device'\n\
         [resource.rig.artifact.app]\n\
         local = 'out/app'\n\
         device = '/tmp/app'\n\
         digest = 'sha256sum \"$BOSUN_ARTIFACT_DEVICE\"'\n\
         timeout = '30s'\n\
         [resource.rig.artifact.lib]\n\
         local = 'out/lib'\n\
         device = '/tmp/lib'\n\
         digest = 'sha256sum /tmp/lib'\n",
        &Local,
    )
    .unwrap();
    assert_eq!(config.notices, []);
    let rig = &config.resources["rig"];
    let app = &rig.artifacts["app"];
    assert_eq!(
        (app.local.as_str(), app.device.as_str(), app.digest.as_str()),
        (
            "out/app",
            "/tmp/app",
            "sha256sum \"$BOSUN_ARTIFACT_DEVICE\""
        )
    );
    assert_eq!(app.timeout, Duration::from_secs(30));
    assert_eq!(rig.artifacts["lib"].timeout, Duration::from_mins(2));
    assert!(
        parse("[resource.target]\nkind = 'build-dir'\n", &Local)
            .unwrap()
            .resources["target"]
            .artifacts
            .is_empty()
    );

    let found = problems(
        "[resource.rig]\n\
         kind = 'device'\n\
         [resource.rig.artifact.nodigest]\n\
         local = 'out/app'\n\
         device = '/tmp/app'\n\
         [resource.rig.artifact.nolocal]\n\
         device = '/tmp/app'\n\
         digest = 'true'\n\
         [resource.rig.artifact.up]\n\
         local = '../app'\n\
         device = '/tmp/app'\n\
         digest = 'true'\n\
         [resource.rig.artifact.pushing]\n\
         local = 'out/app'\n\
         device = '/tmp/app'\n\
         digest = 'true'\n\
         push = 'adb push out/app /tmp/app'\n\
         [resource.rig.artifact.'a b']\n\
         local = 'out/app'\n\
         device = '/tmp/app'\n\
         digest = 'true'\n",
    );
    let expected = [
        "`resource.rig.artifact.nodigest` has no `digest`",
        "`resource.rig.artifact.nolocal` has no `local`",
        "`resource.rig.artifact.up.local` is \"../app\"; a relative path has no `..` component",
        "unknown key `resource.rig.artifact.pushing.push`; an artifact has `local`, `device`, \
         `digest` and `timeout`",
        "`resource.rig.artifact.a b`: a name is ASCII letters, digits, `-` and `_`",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for expected in expected {
        assert!(
            found.iter().any(|found| found.contains(expected)),
            "{expected:?} is not in {found:#?}"
        );
    }
}

#[test]
fn a_resource_probe_has_a_command_and_a_timeout() {
    let config = parse(
        "[resource.rig]\n\
         kind = 'device'\n\
         probe = 'adb get-state'\n\
         probe_timeout = '5s'\n\
         [resource.slow]\n\
         kind = 'device'\n\
         probe = 'true'\n\
         probe_timeout = '60s'\n\
         [resource.plain]\n\
         kind = 'device'\n\
         probe = 'true'\n\
         [resource.none]\n\
         kind = 'gpu'\n",
        &Local,
    )
    .unwrap();
    assert_eq!(config.notices, []);
    let rig = &config.resources["rig"];
    assert_eq!(rig.probe.as_deref(), Some("adb get-state"));
    assert_eq!(rig.probe_timeout, Duration::from_secs(5));
    assert_eq!(
        config.resources["slow"].probe_timeout,
        Duration::from_secs(60)
    );
    assert_eq!(
        config.resources["plain"].probe_timeout,
        Duration::from_secs(10)
    );
    assert_eq!(config.resources["none"].probe, None);

    let both = parse(
        "[resource.rig]\n\
         kind = 'device'\n\
         probe = 'adb get-state'\n\
         [resource.rig.artifact.app]\n\
         local = 'out/app'\n\
         device = '/tmp/app'\n\
         digest = 'true'\n",
        &Local,
    )
    .unwrap();
    let rig = &both.resources["rig"];
    assert_eq!(rig.probe.as_deref(), Some("adb get-state"));
    assert_eq!(rig.artifacts["app"].device, "/tmp/app");

    let found = problems(
        "[resource.a]\n\
         kind = 'device'\n\
         probe_timeout = '5s'\n\
         [resource.b]\n\
         kind = 'device'\n\
         probe = ''\n\
         [resource.c]\n\
         kind = 'device'\n\
         probe = 'true'\n\
         probe_timeout = '2m'\n",
    );
    let expected = [
        "line 3, column 1: `resource.a.probe_timeout` without `probe`",
        "line 6, column 1: `resource.b.probe` is empty",
        "line 10, column 1: `resource.c.probe_timeout` is 120s",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for (found, expected) in found.iter().zip(expected) {
        assert!(found.starts_with(expected), "{found:?} is not {expected:?}");
    }
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
                shared_build: None,
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

/// `[resource.target]`, exclusive, for the repository's shared build directory.
const TARGET_RESOURCE: &str = "[resource.target]\nkind = 'build-dir'\n";

#[test]
fn landing_keys_are_read() {
    let text = with_repo(&format!(
        "landing = {{ mode = \"merge\", target = \"main\", merge = {{ method = \"squash\", \
         push = false }}, pr = {{ draft = false, reviewers = ['someone'], \
         labels = ['bosun', 'needs review'] }} }}\n\
         worktree = {{ root = '{}', setup = \"cargo fetch\", disk_floor = \"20GB\", \
         shared_build = {{ dir = '{}', resource = \"target\", env = \"CARGO_TARGET_DIR\" }} }}\n\
         {TARGET_RESOURCE}",
        abs("wt/demo"),
        abs("wt/demo-target"),
    ));
    let config = parse(&text, &Local).unwrap();
    assert_eq!(config.notices, []);
    let repo = &config.repos["bosun"];
    assert_eq!(
        repo.landing,
        Landing {
            mode: LandingMode::Merge,
            target: Some("main".to_owned()),
            merge: Merge {
                method: MergeMethod::Squash,
                push: false,
            },
            pr: PullRequest {
                draft: false,
                reviewers: vec!["someone".to_owned()],
                labels: vec!["bosun".to_owned(), "needs review".to_owned()],
            },
        }
    );
    assert_eq!(
        repo.worktree,
        Some(Worktree {
            root: PathBuf::from(abs("wt/demo")),
            setup: Some("cargo fetch".to_owned()),
            teardown: None,
            disk_floor: Some(20_000_000_000),
            shared_build: Some(SharedBuild {
                dir: PathBuf::from(abs("wt/demo-target")),
                resource: "target".to_owned(),
                env: "CARGO_TARGET_DIR".to_owned(),
            }),
        })
    );
    for (method, expected) in [("ff", MergeMethod::Ff), ("no-ff", MergeMethod::NoFf)] {
        let text = with_repo(&format!(
            "landing = {{ mode = 'merge', merge = {{ method = '{method}' }} }}\n\
             worktree = {{ root = '{}' }}\n",
            abs("wt/demo")
        ));
        let config = parse(&text, &Local).unwrap();
        assert_eq!(config.repos["bosun"].landing.merge.method, expected);
    }

    // The defaults, with no `landing` at all and with an empty one.
    for rest in ["", "landing = {}\n"] {
        let config = parse(&with_repo(rest), &Local).unwrap();
        let landing = &config.repos["bosun"].landing;
        assert_eq!(landing.mode, LandingMode::Local, "{rest:?}");
        assert_eq!(landing.target, None);
        assert_eq!(landing.merge.method, MergeMethod::Ff);
        assert!(!landing.merge.push);
        assert!(landing.pr.draft);
        assert!(landing.pr.reviewers.is_empty() && landing.pr.labels.is_empty());
        assert_eq!(*landing, Landing::default());
    }
    let text = with_repo(&format!(
        "worktree = {{ root = '{}', shared_build = {{ dir = '{}', resource = 'target' }} }}\n\
         {TARGET_RESOURCE}",
        abs("wt/demo"),
        abs("wt/demo-target"),
    ));
    let config = parse(&text, &Local).unwrap();
    let worktree = config.repos["bosun"].worktree.as_ref().unwrap();
    let shared = worktree.shared_build.as_ref().unwrap();
    assert_eq!(shared.env, "CARGO_TARGET_DIR");
    assert_eq!(shared.env, DEFAULT_SHARED_BUILD_ENV);
}

/// E19: every new landing path is gated on `repo.<name>.worktree`. `pr` is accepted with one;
/// `merge.push` is read and refused until a later card. `mutant_config_landing_without_worktree`
/// accepts `merge` without a worktree.
#[test]
fn merge_and_pr_need_a_worktree() {
    let worktree = format!("worktree = {{ root = '{}' }}\n", abs("wt/bosun"));

    let found = problems(&with_repo("landing = { mode = 'merge' }\n"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("line 7, column 20: `repo.bosun.landing.mode` is \"merge\"")
            && found[0].contains("needs `repo.bosun.worktree`"),
        "{found:?}"
    );
    let config = parse(
        &with_repo(&format!("landing = {{ mode = 'merge' }}\n{worktree}")),
        &Local,
    )
    .unwrap();
    assert_eq!(config.repos["bosun"].landing.mode, LandingMode::Merge);
    // `local` needs none: stage 1 is unchanged.
    parse(&with_repo("landing = { mode = 'local' }\n"), &Local).unwrap();

    let config = parse(
        &with_repo(&format!("landing = {{ mode = 'pr' }}\n{worktree}")),
        &Local,
    )
    .unwrap();
    assert_eq!(config.repos["bosun"].landing.mode, LandingMode::Pr);
    let found = problems(&with_repo("landing = { mode = 'pr' }\n"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found
            .iter()
            .any(|f| f.contains("needs `repo.bosun.worktree`")),
        "{found:?}"
    );

    let found = problems(&with_repo(&format!(
        "landing = {{ mode = 'merge', merge = {{ push = true }} }}\n{worktree}"
    )));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("line 7, column 46: `repo.bosun.landing.merge.push` is true")
            && found[0].contains("not available yet"),
        "{found:?}"
    );
}

/// E1: the shared build directory is guarded by an exclusive lease and lies outside the
/// checkout and the worktrees. `mutant_config_shared_build_any_resource` skips the resource.
#[test]
fn a_shared_build_names_an_exclusive_resource_and_a_directory_outside_the_repository() {
    let text = |dir: &str, resource: &str, resources: &str| {
        with_repo(&format!(
            "worktree = {{ root = '{}', shared_build = {{ dir = '{}', resource = '{resource}' \
             }} }}\n{resources}",
            abs("wt/bosun"),
            abs(dir),
        ))
    };
    let outside = "wt/bosun-target";

    let config = parse(&text(outside, "target", TARGET_RESOURCE), &Local).unwrap();
    let worktree = config.repos["bosun"].worktree.as_ref().unwrap();
    assert_eq!(
        worktree.shared_build.as_ref().map(|s| s.resource.as_str()),
        Some("target")
    );

    let found = problems(&text(outside, "missing", TARGET_RESOURCE));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains(
            "`repo.bosun.worktree.shared_build.resource` names `resource.missing`, which is \
             not configured"
        ),
        "{found:?}"
    );
    let found = problems(&text(
        outside,
        "target",
        "[resource.target]\nkind = 'build-dir'\ncapacity = 2\n",
    ));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("names `resource.target`, whose capacity is 2")
            && found[0].contains("`capacity = 1`"),
        "{found:?}"
    );

    // Under the repository (whatever the case on Windows), the repository itself, and under
    // the worktree root.
    let in_repo = if cfg!(windows) {
        "GIT/Bosun/target"
    } else {
        "git/bosun/target"
    };
    for (dir, owner) in [
        (in_repo, "inside the repository's path"),
        ("git/bosun", "inside the repository's path"),
        ("wt/bosun/target", "inside `worktree.root`"),
    ] {
        let found = problems(&text(dir, "target", TARGET_RESOURCE));
        assert_eq!(found.len(), 1, "{dir}: {found:?}");
        assert!(
            found[0].contains("`repo.bosun.worktree.shared_build.dir`") && found[0].contains(owner),
            "{dir}: {found:?}"
        );
    }

    // Its own keys are checked too.
    let found = problems(&with_repo(&format!(
        "worktree = {{ root = '{}', shared_build = {{ env = '1X', dirs = 'x' }} }}\n",
        abs("wt/bosun"),
    )));
    let expected = [
        "`repo.bosun.worktree.shared_build.env` is \"1X\"",
        "unknown key `repo.bosun.worktree.shared_build.dirs`",
        "`repo.bosun.worktree.shared_build` has no `dir`",
        "`repo.bosun.worktree.shared_build` has no `resource`",
    ];
    assert_eq!(found.len(), expected.len(), "{found:#?}");
    for expected in expected {
        assert!(
            found.iter().any(|f| f.contains(expected)),
            "{found:#?} lacks {expected:?}"
        );
    }
}

/// E1: a shared build directory that holds the checkout or the worktrees gets cargo's files
/// next to them, and a `cargo clean` there deletes them. `mutant_config_shared_build_holder_allowed`
/// checks only the "under" direction.
#[test]
fn a_shared_build_holding_the_repository_or_the_worktree_root_is_refused() {
    let text = |dir: &str| {
        with_repo(&format!(
            "worktree = {{ root = '{}', shared_build = {{ dir = '{}', resource = 'target' }} \
             }}\n{TARGET_RESOURCE}",
            abs("wt/bosun"),
            abs(dir),
        ))
    };
    for (dir, holds) in [
        ("wt", "holds `worktree.root`"),
        ("git", "holds the repository's path"),
    ] {
        let found = problems(&text(dir));
        assert_eq!(found.len(), 1, "{dir}: {found:?}");
        assert!(
            found[0].contains("`repo.bosun.worktree.shared_build.dir`") && found[0].contains(holds),
            "{dir}: {found:?}"
        );
    }
    let config = parse(&text("wt/bosun-target"), &Local).unwrap();
    let worktree = config.repos["bosun"].worktree.as_ref().unwrap();
    assert_eq!(
        worktree.shared_build.as_ref().map(|s| s.dir.clone()),
        Some(PathBuf::from(abs("wt/bosun-target")))
    );
}

/// E19: only worktree mode reads `landing.target`; without a worktree it is ignored with a
/// notice, as before #149. `mutant_config_target_kept_without_worktree` keeps it silently.
#[test]
fn a_target_without_a_worktree_is_ignored_with_a_notice() {
    let config = parse(&with_repo("landing = { target = 'main' }\n"), &Local).unwrap();
    assert_eq!(config.repos["bosun"].landing.target, None);
    let notices: Vec<String> = config
        .notices
        .iter()
        .map(ToString::to_string)
        .filter(|n| n.contains("`repo.bosun.landing.target`"))
        .collect();
    assert_eq!(notices.len(), 1, "{:?}", config.notices);
    assert!(notices[0].contains("worktree"), "{notices:?}");

    let worktree = format!("worktree = {{ root = '{}' }}\n", abs("wt/bosun"));
    for landing in [
        "landing = { target = 'main' }\n",
        "landing = { mode = 'local', target = 'main' }\n",
    ] {
        for text in [
            with_repo(&format!("{landing}{worktree}")),
            with_repo(&format!("{worktree}{landing}")),
        ] {
            let config = parse(&text, &Local).unwrap();
            assert_eq!(
                config.repos["bosun"].landing.target.as_deref(),
                Some("main"),
                "{text}"
            );
            assert!(
                !config
                    .notices
                    .iter()
                    .any(|n| n.to_string().contains("target")),
                "{text}: {:?}",
                config.notices
            );
        }
    }
}

/// A target git would refuse at landing time is refused when the file is read, by the rules of
/// `git check-ref-format --branch`. `mutant_config_target_loose_branch_rules` checks the old few.
#[test]
fn a_landing_target_follows_gits_branch_name_rules() {
    let text = |target: &str| {
        with_repo(&format!(
            "landing = {{ target = \"{}\" }}\nworktree = {{ root = '{}' }}\n",
            target
                .chars()
                .map(|c| match c {
                    '\\' | '"' => format!("\\{c}"),
                    c if c.is_control() => format!("\\u{:04X}", u32::from(c)),
                    c => c.to_string(),
                })
                .collect::<String>(),
            abs("wt/bosun"),
        ))
    };
    for bad in [
        "a..b",
        "x.lock",
        "a~1",
        "a^",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        r"a\b",
        "a//b",
        "/a",
        "a/",
        ".a",
        "a/.b",
        "a.",
        "@",
        "a@{b",
        "a\u{1}b",
        "HEAD",
        "-x",
        "refs/heads/x",
        "",
        "a b",
        "a\u{7f}b",
    ] {
        let found = problems(&text(bad));
        assert_eq!(found.len(), 1, "{bad:?}: {found:?}");
        assert!(
            found[0].contains("`repo.bosun.landing.target`"),
            "{bad:?}: {found:?}"
        );
    }
    for good in ["main", "feature/x", "release-1.2", "user@host", "a.b/c_d"] {
        let config = parse(&text(good), &Local).unwrap();
        assert_eq!(config.repos["bosun"].landing.target.as_deref(), Some(good));
    }
}

#[test]
fn the_repository_hash_covers_landing_and_the_worktree() {
    let base = with_repo(&format!(
        "landing = {{ mode = 'merge', merge = {{ method = 'ff' }} }}\n\
         worktree = {{ root = '{}', shared_build = {{ dir = '{}', resource = 'target' }} }}\n\
         {TARGET_RESOURCE}",
        abs("wt/bosun"),
        abs("wt/bosun-target"),
    ));
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let before = hash(&base);
    assert_eq!(hash(&base.replace("method = 'ff'", "method='ff'")), before);
    for edit in [
        base.replace("method = 'ff'", "method = 'squash'"),
        base.replace("method = 'ff'", "method = 'no-ff'"),
        base.replace("mode = 'merge',", "mode = 'merge', target = 'main',"),
        base.replace("mode = 'merge',", "mode = 'merge', pr = { draft = false },"),
        base.replace(
            "mode = 'merge',",
            "mode = 'merge', pr = { labels = ['x'] },",
        ),
        base.replace("resource = 'target'", "resource = 'target', env = 'TARGET'"),
        base.replace("bosun-target", "bosun-build"),
        base.replace("root = '", "setup = 'cargo fetch', root = '"),
    ] {
        assert_ne!(hash(&edit), before, "{edit}");
    }
}

#[test]
fn a_stage_1_repository_keeps_its_hash() {
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let bare = hash(&with_repo(""));
    // Both literals were computed with the encoding before #149 (commit `fd4be74`); `abs`
    // makes the paths, and so the hashes, platform-specific.
    #[cfg(windows)]
    {
        assert_eq!(
            bare,
            "91cb6c9e80fc5851def90d18c246486871b762bc2f72605426c7eadc93425733"
        );
        assert_eq!(
            hash(&with_repo(
                "landing = { mode = 'local' }
model = 'opus'
verdict = { timeout = '5m' }
"
            )),
            "c77dbbb84a1535669de33ac3b620ed866a8548cfdbe1767cb36364b41adeb8bd"
        );
    }
    for same in [
        "landing = { mode = 'local' }
",
        // Ignored without a worktree (E19).
        "landing = { target = 'main' }
",
        "landing = { pr = { draft = true, reviewers = [], labels = [] } }
",
    ] {
        assert_eq!(hash(&with_repo(same)), bare, "{same}");
    }
    let worktree = with_repo(&format!(
        "worktree = {{ root = '{}' }}
",
        abs("wt/bosun")
    ));
    assert_ne!(hash(&worktree), bare);
}

#[test]
fn a_sweep_lists_process_names_and_refuses_anything_else() {
    let text =
        with_repo("sweep = { processes = ['devserver', 'emu-1.exe', 'a_b', 'devserver'] }\n");
    let config = parse(&text, &Local).unwrap();
    let sweep = config.repos["bosun"].sweep.as_ref().expect("a sweep");
    assert_eq!(
        sweep.processes,
        BTreeSet::from(["a_b", "devserver", "emu-1.exe"].map(String::from))
    );
    let bare = parse(&with_repo(""), &Local).unwrap();
    assert_eq!(bare.repos["bosun"].sweep, None);
    let name = "a process name is a file name";
    for (rest, expected) in [
        (
            "sweep = { process = ['x'] }\n",
            "unknown key `repo.bosun.sweep.process`",
        ),
        ("sweep = { processes = [''] }\n", name),
        ("sweep = { processes = ['bin/x'] }\n", name),
        ("sweep = { processes = ['a\\b'] }\n", name),
        ("sweep = { processes = ['..'] }\n", name),
        ("sweep = { processes = ['dev server'] }\n", name),
        ("sweep = { processes = [1] }\n", "must hold strings only"),
        (
            "sweep = { processes = 'x' }\n",
            "must be an array of strings",
        ),
        ("sweep = 'x'\n", "repo.bosun.sweep"),
    ] {
        let found = problems(&with_repo(rest));
        assert!(
            found.iter().any(|p| p.contains(expected)),
            "{rest}: {found:?}"
        );
    }
}

#[test]
fn the_repository_hash_covers_the_sweep_only_when_it_is_set() {
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let bare = hash(&with_repo(""));
    let swept = hash(&with_repo("sweep = { processes = ['devserver'] }\n"));
    assert_ne!(swept, bare);
    let twice = with_repo("sweep = { processes = ['devserver', 'devserver'] }\n");
    assert_eq!(hash(&twice), swept);
    let other = with_repo("sweep = { processes = ['emulator'] }\n");
    assert_ne!(hash(&other), swept);
    assert_ne!(hash(&with_repo("sweep = {}\n")), bare);
    let worktree = format!("worktree = {{ root = '{}' }}\n", abs("wt/bosun"));
    let both = format!("{worktree}sweep = {{ processes = ['devserver'] }}\n");
    assert_ne!(hash(&with_repo(&both)), hash(&with_repo(&worktree)));
}

#[test]
fn an_unknown_repository_effort_level_is_refused() {
    let found = problems(&with_repo("effort = 'ultra'\n"));
    assert!(
        found.iter().any(|p| p.contains("repo.bosun.effort")
            && p.contains("ultra")
            && bosun_core::EFFORT_LEVELS.iter().all(|l| p.contains(l))),
        "{found:?}"
    );
    for rest in ["effort = ''\n", "effort = 'High'\n", "effort = 3\n"] {
        let found = problems(&with_repo(rest));
        assert!(
            found.iter().any(|p| p.contains("repo.bosun.effort")),
            "{rest}: {found:?}"
        );
    }
    let config = parse(&with_repo("effort = 'max'\n"), &Local).unwrap();
    assert_eq!(config.repos["bosun"].effort.as_deref(), Some("max"));
    let bare = parse(&with_repo(""), &Local).unwrap();
    assert_eq!(bare.repos["bosun"].effort, None);

    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let plain = hash(&with_repo(""));
    let high = hash(&with_repo("effort = 'high'\n"));
    assert_ne!(high, plain);
    assert_ne!(hash(&with_repo("effort = 'low'\n")), high);
    let sweep = "sweep = { processes = ['devserver'] }\n";
    assert_ne!(
        hash(&with_repo(&format!("{sweep}effort = 'high'\n"))),
        hash(&with_repo(sweep))
    );
}

#[test]
fn the_fast_death_keys_change_the_hash_only_when_not_default() {
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let with = |daemon: &str| format!("[daemon]\npark_cap = 4\n{daemon}{}", with_repo(""));
    let bare = hash(&with(""));
    // At their defaults the keys leave the hash what the encoding before them gives, computed
    // here from the same configuration, so this holds on every platform.
    let before = parse(&with(""), &Local)
        .unwrap()
        .repo_hash_without_fast_death("bosun")
        .unwrap();
    assert_eq!(bare, before, "the encoding before the fast-death keys");
    // And pinned, as an extra: `abs` makes the paths, and so the value, platform-specific.
    #[cfg(windows)]
    assert_eq!(
        bare,
        "3d65afa11d6cf33ee8b6f6dfb843c2a55f836f9b4fe9e0d0e4fc342883f68500"
    );
    assert_eq!(
        hash(&with(
            "fast_death_sessions = 3\nfast_death_within = '60s'\n"
        )),
        bare,
        "the defaults written out"
    );
    assert_eq!(hash(&with("fast_death_within = '1m'\n")), bare);
    let sessions = hash(&with("fast_death_sessions = 4\n"));
    let within = hash(&with("fast_death_within = '90s'\n"));
    assert_ne!(sessions, bare);
    assert_ne!(within, bare);
    assert_ne!(sessions, within);
}

#[test]
fn a_census_policy_reads_its_glob_and_counters() {
    let policy = "[repo.bosun.policy]\nprotected = ['tests/harness']\n";
    let census = "[repo.bosun.policy.census]\n\
                  glob = 'tests/**'\n\
                  keep = { tests = 'fn test_', assertions = 'assert' }\n\
                  cap = { ignored = '#\\[ignore' }\n";
    let with = parse(&with_repo(&format!("{policy}{census}")), &Local).unwrap();
    let read = with.repos["bosun"].policy.census.clone().expect("a census");
    let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    };
    assert_eq!(
        read,
        Census::new(
            "tests/**".to_owned(),
            map(&[("tests", "fn test_"), ("assertions", "assert")]),
            map(&[("ignored", r"#\[ignore")]),
        )
    );

    for (bad, what) in [
        (
            "glob = 'tests/**'\nkeep = { a = 'x' }\nskip = 1\n",
            "unknown key",
        ),
        ("glob = ''\nkeep = { a = 'x' }\n", "glob"),
        (
            "glob = 'tests/**'\nkeep = { a = 'x' }\ncap = { a = 'y' }\n",
            "both",
        ),
        ("glob = 'tests/**'\n", "no counter"),
    ] {
        let found = problems(&with_repo(&format!(
            "{policy}[repo.bosun.policy.census]\n{bad}"
        )));
        assert_eq!(found.len(), 1, "{bad:?}: {found:?}");
        assert!(found[0].contains("census"), "{bad:?}: {found:?}");
        assert!(found[0].contains(what), "{bad:?}: {found:?}");
    }

    // A policy without a census keeps the hash it had before the census existed (pinned), and
    // a census changes it.
    let without = parse(&with_repo(policy), &Local).unwrap();
    let without = without.repos["bosun"].policy.hash();
    assert_eq!(
        without,
        "6b58c1cfd201e82a3efcb3e28d636bb72f3e388a6d40fbfa7aedb08473d9260d"
    );
    assert_ne!(with.repos["bosun"].policy.hash(), without);
}

#[test]
fn a_repository_auto_pause_defaults_to_the_global_value_without_a_worktree_and_off_with_one() {
    let worktree = format!("worktree = {{ root = '{}' }}\n", abs("wt/bosun"));
    let resolved = |daemon: &str, rest: &str| {
        let config = parse(&format!("{daemon}{}", with_repo(rest)), &Local).unwrap();
        config.repos["bosun"].auto_pause(&config.daemon)
    };
    let off = "[daemon]\nauto_pause = false\n";
    // Without a worktree: the global value.
    assert!(resolved("", ""));
    assert!(!resolved(off, ""));
    // With one: off, whatever the global value (mutant_config_auto_pause_worktree_default_on).
    assert!(!resolved("", &worktree));
    assert!(!resolved(off, &worktree));
    // The repository's own value wins.
    assert!(resolved("", &format!("{worktree}auto_pause = true\n")));
    assert!(resolved(off, &format!("{worktree}auto_pause = true\n")));
    assert!(!resolved("", "auto_pause = false\n"));
    assert!(resolved(off, "auto_pause = true\n"));
    let config = parse(&with_repo("auto_pause = false\n"), &Local).unwrap();
    assert_eq!(config.repos["bosun"].auto_pause, Some(false));
    assert!(config.notices.is_empty(), "{:?}", config.notices);

    let found = problems(&with_repo("auto_pause = 'yes'\n"));
    assert!(
        found.iter().any(|p| p.contains("repo.bosun.auto_pause")),
        "{found:?}"
    );

    // Not part of the configuration hash (mutant_config_auto_pause_hashed).
    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    for base in [String::new(), worktree] {
        let bare = hash(&with_repo(&base));
        for value in ["true", "false"] {
            let set = with_repo(&format!("{base}auto_pause = {value}\n"));
            assert_eq!(hash(&set), bare, "{set}");
        }
    }
}

/// #204: the not-ready keys of a verdict are off by default, a code that means something else
/// is refused, and the repository hash covers them only when one is not at its default.
#[test]
fn the_verdict_unready_keys_are_read_checked_and_hashed_only_when_set() {
    let set = "verdict = { unready_exit = 75, unready_backoff = \"10m\", unready_cap = 3 }\n";
    let config = parse(&with_repo(set), &Local).unwrap();
    let verdict = &config.repos["bosun"].verdict;
    assert_eq!(verdict.unready_exit, Some(75));
    assert_eq!(verdict.unready_backoff, Duration::from_mins(10));
    assert_eq!(verdict.unready_cap, 3);

    let bare = parse(&with_repo(""), &Local).unwrap();
    let verdict = &bare.repos["bosun"].verdict;
    assert_eq!(verdict.unready_exit, None);
    assert_eq!(verdict.unready_backoff, Duration::from_mins(5));
    assert_eq!(verdict.unready_cap, 6);

    for code in ["0", "1", "126", "127", "128", "300", "-75", "'75'"] {
        let found = problems(&with_repo(&format!(
            "verdict = {{ unready_exit = {code} }}\n"
        )));
        assert!(
            found
                .iter()
                .any(|p| p.contains("repo.bosun.verdict.unready_exit")),
            "{code}: {found:?}"
        );
    }
    for code in [2, 75, 125] {
        let text = with_repo(&format!("verdict = {{ unready_exit = {code} }}\n"));
        let config = parse(&text, &Local).unwrap();
        assert_eq!(config.repos["bosun"].verdict.unready_exit, Some(code));
    }
    let found = problems(&with_repo("verdict = { unready_cap = 0 }\n"));
    assert!(
        found
            .iter()
            .any(|p| p.contains("repo.bosun.verdict.unready_cap")),
        "{found:?}"
    );

    let hash = |text: &str| parse(text, &Local).unwrap().repo_hash("bosun").unwrap();
    let plain = hash(&with_repo(""));
    assert_eq!(
        hash(&with_repo("verdict = { unready_backoff = \"5m\" }\n")),
        plain
    );
    assert_eq!(hash(&with_repo("verdict = { unready_cap = 6 }\n")), plain);
    let on = hash(&with_repo("verdict = { unready_exit = 75 }\n"));
    assert_ne!(on, plain);
    assert_ne!(hash(&with_repo("verdict = { unready_exit = 76 }\n")), on);
    assert_ne!(hash(&with_repo("verdict = { unready_cap = 3 }\n")), plain);
}
