//! The acceptance tests of #54. Every git run here is under a temporary directory with its own
//! configuration files: the user's git configuration and credential store are never read.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{
    Author, Bundle, Env, NO_SSH, NO_TOKEN, ProbeError, Refusal, RemoteUrl, ResolvedAuthor,
    TokenLogin, before_spawn, before_write, inherited,
};

const SENTINEL: &str = "bosun-sentinel-credential-7f3a";
const CHILD: &str = "BOSUN_IDENTITY_SENTINEL_CHILD";

fn author() -> Author {
    Author {
        name: "Test Identity".to_owned(),
        email: "identity@example.test".to_owned(),
    }
}

fn bundle(dir: &Path) -> Bundle {
    Bundle {
        identity: "personal".to_owned(),
        host: "github.com".to_owned(),
        login: "test-login".to_owned(),
        gh_config_dir: dir.join("gh"),
        git_config: dir.join("identity.gitconfig"),
        ssh_key: Some(dir.join("id_test_key")),
        author: author(),
        owners: vec!["test-owner".to_owned(), "test-org".to_owned()],
        runner_dirs: vec![("CLAUDE_CONFIG_DIR".to_owned(), dir.join("claude"))],
        runner_env: Vec::new(),
    }
}

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// The command that prints its own environment.
fn print_env() -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "set"]);
        command
    } else {
        Command::new("env")
    }
}

#[test]
fn a_sentinel_credential_in_the_daemon_environment_is_absent_in_the_child() {
    // The daemon's own environment cannot be changed from a test without `unsafe`, so the test
    // binary runs itself with the sentinel set: that process is the daemon.
    let exe = std::env::current_exe().expect("the test binary");
    let out = Command::new(exe)
        .args([
            "tests::sentinel_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("GH_TOKEN", SENTINEL)
        .env("ANTHROPIC_API_KEY", SENTINEL)
        .env("BOSUN_TEST_SENTINEL", SENTINEL)
        .stdin(Stdio::null())
        .output()
        .expect("the test binary runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("1 passed"),
        "the child test did not run: {text}"
    );
}

/// The daemon side of the sentinel test; does nothing unless that test started it.
#[test]
fn sentinel_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle(dir.path());
    let inherited = inherited();
    assert!(
        inherited.iter().any(|(_, v)| v == SENTINEL),
        "the sentinel is in the daemon's environment"
    );
    let declared = names(&["GH_TOKEN", "ANTHROPIC_API_KEY"]);
    for (which, env) in [
        ("tool", Env::tool(&bundle, &inherited)),
        (
            "agent",
            Env::agent(&bundle, &inherited, &declared, dir.path()),
        ),
    ] {
        let mut command = print_env();
        env.apply(&mut command);
        let out = command.output().expect("the environment is printed");
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(
            printed
                .lines()
                .any(|l| l.to_ascii_uppercase().starts_with("PATH=")),
            "{which}: the child got the essentials: {printed}"
        );
        assert!(
            !printed.contains(SENTINEL),
            "{which}: the sentinel reached the child: {printed}"
        );
    }
}

#[test]
fn the_agent_environment_has_no_github_credential_of_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle(dir.path());
    let empty = dir.path().join("empty");
    let inherited = os(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/test"),
        ("GH_TOKEN", SENTINEL),
        ("GITHUB_TOKEN", SENTINEL),
        ("SSH_AUTH_SOCK", "/tmp/ssh-agent.sock"),
        ("GH_CONFIG_DIR", "/home/test/.config/gh"),
    ]);
    let agent = Env::agent(&bundle, &inherited, &[], &empty);

    assert_eq!(agent.get("GH_CONFIG_DIR"), Some(empty.as_os_str()));
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        assert_eq!(agent.get(name), Some(NO_TOKEN.as_ref()), "{name}");
    }
    assert_eq!(agent.get("SSH_AUTH_SOCK"), None);
    assert_eq!(agent.get("GIT_SSH_COMMAND"), Some(NO_SSH.as_ref()));
    let key = bundle
        .ssh_key
        .as_ref()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let gh = bundle.gh_config_dir.as_os_str();
    for (name, value) in agent.iter() {
        assert_ne!(value, SENTINEL, "{name}");
        assert_ne!(value, gh, "{name} is the identity's gh configuration");
        assert!(
            !value.to_string_lossy().contains(&key),
            "{name} names the key"
        );
    }
    // What it does get: the author and signing configuration, the runner's directory.
    assert_eq!(
        agent.get("GIT_CONFIG_GLOBAL"),
        Some(bundle.git_config.as_os_str())
    );
    assert_eq!(
        agent.get("CLAUDE_CONFIG_DIR"),
        Some(dir.path().join("claude").as_os_str())
    );

    // The daemon's own git and gh do get the bundle, and no runner directory.
    let tool = Env::tool(&bundle, &inherited);
    assert_eq!(tool.get("GH_CONFIG_DIR"), Some(gh));
    assert!(
        tool.get("GIT_SSH_COMMAND")
            .is_some_and(|c| c.to_string_lossy().contains(&key))
    );
    assert_eq!(tool.get("GH_TOKEN"), None);
    assert_eq!(tool.get("CLAUDE_CONFIG_DIR"), None);
}

#[test]
fn a_repository_asking_for_gh_token_does_not_get_it() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle(dir.path());
    let inherited = os(&[
        ("PATH", "/usr/bin"),
        ("GH_TOKEN", SENTINEL),
        ("OPENAI_API_KEY", SENTINEL),
        ("CARGO_REGISTRY_TOKEN", SENTINEL),
        ("GH_CONFIG_DIR", "/home/test/.config/gh"),
        ("CARGO_HOME", "/home/test/.cargo"),
    ]);
    let declared = names(&[
        "GH_TOKEN",
        "OPENAI_API_KEY",
        "CARGO_REGISTRY_TOKEN",
        "GH_CONFIG_DIR",
        "CARGO_HOME",
    ]);
    let env = Env::agent(&bundle, &inherited, &declared, &dir.path().join("empty"));

    assert_eq!(env.get("GH_TOKEN"), Some(NO_TOKEN.as_ref()));
    assert_eq!(env.get("OPENAI_API_KEY"), None);
    assert_eq!(env.get("CARGO_REGISTRY_TOKEN"), None);
    assert_eq!(env.get("CARGO_HOME"), Some("/home/test/.cargo".as_ref()));
    assert_eq!(
        env.denied(),
        &names(&[
            "GH_TOKEN",
            "OPENAI_API_KEY",
            "CARGO_REGISTRY_TOKEN",
            "GH_CONFIG_DIR"
        ])[..]
    );
}

#[test]
fn the_identity_supplies_its_runner_credentials_but_never_a_github_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut bundle = bundle(dir.path());
    bundle.runner_env = vec![
        ("ANTHROPIC_API_KEY".to_owned(), OsString::from("supplied")),
        ("GH_TOKEN".to_owned(), OsString::from(SENTINEL)),
    ];
    bundle
        .runner_dirs
        .push(("GIT_CONFIG_GLOBAL".to_owned(), dir.path().join("x")));
    let env = Env::agent(&bundle, &[], &[], &dir.path().join("empty"));
    assert_eq!(env.get("ANTHROPIC_API_KEY"), Some("supplied".as_ref()));
    assert_eq!(env.get("GH_TOKEN"), Some(NO_TOKEN.as_ref()));
    assert_eq!(
        env.get("GIT_CONFIG_GLOBAL"),
        Some(bundle.git_config.as_os_str())
    );
    assert_eq!(env.denied(), &names(&["GIT_CONFIG_GLOBAL", "GH_TOKEN"])[..]);
}

/// git's spelling of a path in a shell snippet: forward slashes, which Git for Windows' `sh`
/// accepts too.
fn sh_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A throwaway layout: a home directory with its own global configuration, an identity
/// configuration file, both naming a fake credential helper that records each call.
struct Layout {
    dir: tempfile::TempDir,
    bundle: Bundle,
    marker: PathBuf,
    inherited: Vec<(OsString, OsString)>,
}

impl Layout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bundle = bundle(dir.path());
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        let marker = dir.path().join("helper-was-invoked");
        let helper = format!(
            "!f() {{ echo called >> '{}'; echo username=fake; echo password=fake; }}; f",
            sh_path(&marker)
        );
        let helpers = format!(
            "[credential]\n\thelper = \"{helper}\"\n\
             [credential \"https://example.test\"]\n\thelper = \"{helper}\"\n"
        );
        std::fs::write(home.join(".gitconfig"), &helpers).unwrap();
        std::fs::write(home.join(".config/git/config"), &helpers).unwrap();
        std::fs::write(
            &bundle.git_config,
            format!(
                "[user]\n\tname = {}\n\temail = {}\n{helpers}",
                bundle.author.name, bundle.author.email
            ),
        )
        .unwrap();
        // The daemon's environment, with every home pointing into the layout: even a broken
        // environment cannot reach the user's own configuration.
        let mut inherited: Vec<_> = inherited()
            .into_iter()
            .filter(|(k, _)| {
                let k = k.to_string_lossy().to_ascii_uppercase();
                !["HOME", "USERPROFILE", "XDG_CONFIG_HOME"].contains(&k.as_str())
                    && !k.starts_with("GIT_")
            })
            .collect();
        for name in ["HOME", "USERPROFILE"] {
            inherited.push((name.into(), home.clone().into()));
        }
        inherited.push(("XDG_CONFIG_HOME".into(), home.join(".config").into()));
        Self {
            dir,
            bundle,
            marker,
            inherited,
        }
    }

    fn git(&self, env: &Env, args: &[&str], stdin: &str) -> std::process::Output {
        let mut command = Command::new("git");
        env.apply(&mut command);
        command
            .args(args)
            .current_dir(self.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("git runs");
        {
            use std::io::Write as _;
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(stdin.as_bytes()).unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn agent(&self) -> Env {
        Env::agent(
            &self.bundle,
            &self.inherited,
            &[],
            &self.dir.path().join("empty"),
        )
    }
}

#[test]
fn a_fake_credential_helper_in_the_global_config_is_never_invoked_by_the_agents_git() {
    let layout = Layout::new();
    let ask = "protocol=https\nhost=example.test\npath=owner/repo.git\n\n";

    // The control: the daemon's own git does call the identity's helper.
    let tool = Env::tool(&layout.bundle, &layout.inherited);
    let out = layout.git(&tool, &["credential", "fill"], ask);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(layout.marker.exists(), "the control reached the helper");
    std::fs::remove_file(&layout.marker).unwrap();

    let out = layout.git(&layout.agent(), &["credential", "fill"], ask);
    assert!(
        !layout.marker.exists(),
        "the agent's git called the helper: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.status.success(), "the agent's git found a credential");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("password="));
}

#[test]
fn the_agents_git_resolves_the_identitys_author() {
    let layout = Layout::new();
    let out = layout.git(&layout.agent(), &["var", "GIT_AUTHOR_IDENT"], "");
    let line = String::from_utf8_lossy(&out.stdout);
    assert_eq!(Author::from_ident(&line), Some(author()), "{line}");
}

#[test]
fn the_agents_git_cannot_use_ssh() {
    let layout = Layout::new();
    let out = layout.git(
        &layout.agent(),
        &["ls-remote", "ssh://git@example.invalid/owner/repo.git"],
        "",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("bosun: agent sessions have no SSH access"),
        "{stderr}"
    );
}

/// Probes that answer what they were given.
struct Fixed {
    url: Result<&'static str, &'static str>,
    login: Result<&'static str, &'static str>,
    author: Result<Author, &'static str>,
}

impl Fixed {
    fn good() -> Self {
        Self {
            url: Ok("git@github.com:Test-Owner/repo.git"),
            login: Ok("Test-Login"),
            author: Ok(author()),
        }
    }

    fn spawn(&self) -> Result<(), Vec<Refusal>> {
        let dir = tempfile::tempdir().unwrap();
        before_spawn(&bundle(dir.path()), Path::new("repo"), self, self, self)
    }
}

fn probe<T: Clone>(r: &Result<T, &'static str>) -> Result<T, ProbeError> {
    r.clone().map_err(|e| ProbeError(e.to_owned()))
}

impl RemoteUrl for Fixed {
    fn remote_url(&self, _: &Path) -> Result<String, ProbeError> {
        probe(&self.url).map(str::to_owned)
    }
}

impl TokenLogin for Fixed {
    fn login(&self, host: &str) -> Result<String, ProbeError> {
        assert_eq!(host, "github.com");
        probe(&self.login).map(str::to_owned)
    }
}

impl ResolvedAuthor for Fixed {
    fn author(&self, _: &Path) -> Result<Author, ProbeError> {
        probe(&self.author)
    }
}

#[test]
fn a_matching_identity_passes_every_check() {
    assert_eq!(Fixed::good().spawn(), Ok(()));
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(before_write(&bundle(dir.path()), &Fixed::good()), Ok(()));
}

#[test]
fn remote_owner_off_the_allowlist_is_refused() {
    let probes = Fixed {
        url: Ok("https://github.com/stranger/repo.git"),
        ..Fixed::good()
    };
    assert_eq!(
        probes.spawn(),
        Err(vec![Refusal::OwnerNotAllowed {
            owner: "stranger".to_owned()
        }])
    );
}

#[test]
fn a_remote_on_another_host_or_unreadable_is_refused() {
    for url in ["https://example.test/test-owner/repo.git", "C:/src/repo"] {
        let probes = Fixed {
            url: Ok(url),
            ..Fixed::good()
        };
        assert_eq!(
            probes.spawn(),
            Err(vec![Refusal::RemoteElsewhere {
                url: url.to_owned()
            }])
        );
    }
    let probes = Fixed {
        url: Err("no remote"),
        ..Fixed::good()
    };
    assert_eq!(
        probes.spawn(),
        Err(vec![Refusal::RemoteUnreadable(ProbeError(
            "no remote".to_owned()
        ))])
    );
}

#[test]
fn wrong_login_is_refused() {
    let probes = Fixed {
        login: Ok("other-login"),
        ..Fixed::good()
    };
    let wrong = Refusal::WrongLogin {
        found: "other-login".to_owned(),
    };
    assert_eq!(probes.spawn(), Err(vec![wrong.clone()]));
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(before_write(&bundle(dir.path()), &probes), Err(wrong));
}

#[test]
fn wrong_author_is_refused() {
    let found = Author {
        name: "Test Identity".to_owned(),
        email: "other@example.test".to_owned(),
    };
    let probes = Fixed {
        author: Ok(found.clone()),
        ..Fixed::good()
    };
    assert_eq!(probes.spawn(), Err(vec![Refusal::WrongAuthor { found }]));
}

#[test]
fn a_probe_that_cannot_answer_refuses_and_every_refusal_is_reported() {
    let probes = Fixed {
        url: Ok("https://github.com/stranger/repo"),
        login: Err("gh failed"),
        author: Err("no author"),
    };
    assert_eq!(
        probes.spawn(),
        Err(vec![
            Refusal::OwnerNotAllowed {
                owner: "stranger".to_owned()
            },
            Refusal::LoginUnreadable(ProbeError("gh failed".to_owned())),
            Refusal::AuthorUnreadable(ProbeError("no author".to_owned())),
        ])
    );
}

#[test]
fn the_needs_you_text_never_names_what_was_found() {
    let found = "secret-value";
    let refusals = [
        Refusal::RemoteUnreadable(ProbeError(found.to_owned())),
        Refusal::RemoteElsewhere {
            url: format!("https://x:{found}@example.test/o/r"),
        },
        Refusal::OwnerNotAllowed {
            owner: found.to_owned(),
        },
        Refusal::LoginUnreadable(ProbeError(found.to_owned())),
        Refusal::WrongLogin {
            found: found.to_owned(),
        },
        Refusal::AuthorUnreadable(ProbeError(found.to_owned())),
        Refusal::WrongAuthor {
            found: Author {
                name: found.to_owned(),
                email: format!("{found}@example.test"),
            },
        },
    ];
    for refusal in refusals {
        let text = refusal.to_string();
        assert!(!text.contains(found), "{text}");
    }
}
