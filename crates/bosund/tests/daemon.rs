//! The daemon binary against its endpoint (#24): a second instance, identity isolation, the
//! socket's mode, a clean stop, and the event stream of a reingest (#69). Each test runs its own daemon on its own endpoint, over
//! copies of the synthetic fixtures (#16) in a temporary directory.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bosun_api::rpc::code;
use bosun_api::{EventBody, EventFilter, IdentityParams, TokensParams, method};
use bosun_client::{Client, Delivery, Endpoint, Error, Subscription};
use serde_json::Value;
use tempfile::TempDir;

/// How long a daemon may take to start or stop.
const PATIENCE: Duration = Duration::from_secs(30);

fn fixture(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude")
        .join(case)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A temporary home: identities whose transcripts are copies of fixture cases, a
/// configuration naming them, and an endpoint of its own.
struct Home {
    dir: TempDir,
    endpoint: Endpoint,
}

impl Home {
    /// `identities`: each identity's name and the fixture case its transcripts come from.
    fn new(identities: &[(&str, &str)]) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = tempfile::tempdir().unwrap();
        let mut config = String::new();
        for (name, case) in identities {
            let claude = dir.path().join(name).join("claude");
            copy_dir(&fixture(case).join("projects"), &claude.join("projects"));
            let database = dir.path().join(name).join("bosun.db");
            let _ = write!(
                config,
                "[identity.{name}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
                claude.display(),
                database.display()
            );
        }
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let endpoint = if cfg!(windows) {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            Endpoint::new(format!("bosun-test-{}-{n}", std::process::id()))
        } else {
            Endpoint::new(dir.path().join("bosund.sock"))
        };
        Self { dir, endpoint }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosund"));
        command
            .env("BOSUN_CONFIG", self.dir.path().join("config.toml"))
            .env("BOSUN_ENDPOINT", self.endpoint.path())
            .env(
                "BOSUN_OPERATOR_FILE",
                self.dir.path().join("operator.token"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    #[cfg(unix)]
    fn database(&self, identity: &str) -> PathBuf {
        self.dir.path().join(identity).join("bosun.db")
    }

    /// Starts a daemon and waits until it answers `hello`.
    async fn start(&self) -> Daemon {
        let mut child = self.command().spawn().unwrap();
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "the daemon exited at start: {status}: {}",
                    stderr(&mut child)
                );
            }
            match Client::connect(&self.endpoint).await {
                Ok(_) => return Daemon { child },
                Err(e) if Instant::now() > deadline => panic!("the daemon never answered: {e}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.endpoint).await.unwrap()
    }
}

fn stderr(child: &mut Child) -> String {
    use std::io::Read;
    let mut text = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut text);
    }
    text
}

/// Waits for `child` to exit; `None` if it is still running after `PATIENCE`.
async fn wait(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// A running daemon; killed when dropped.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn a_second_instance_exits() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    let first = home.start().await;

    let mut second = home.command().spawn().unwrap();
    let Some(status) = wait(&mut second).await else {
        let _ = second.kill();
        let _ = second.wait();
        panic!("the second instance is still running");
    };
    let message = stderr(&mut second);
    assert_eq!(status.code(), Some(4), "{message}");
    assert!(
        message.contains(&format!(
            "bosund: another instance is already running at {}",
            home.endpoint
        )),
        "{message}"
    );
    assert!(
        message.contains(&format!("(pid {})", first.pid())),
        "{message}"
    );

    let client = home.client().await;
    assert_eq!(client.daemon().pid, first.pid(), "the first one answers");
    assert_eq!(client.daemon().api_version, bosun_api::API_VERSION);

    // Both log lines survive in the log file beside the configuration (#137), not on stderr
    // only.
    let log = bosun_client::log::path(&home.dir.path().join("config.toml"), &home.endpoint);
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        text.contains("bosund: logging to"),
        "{}: {text}",
        log.display()
    );
    assert!(
        text.contains("bosund: another instance is already running at"),
        "{}: {text}",
        log.display()
    );
}

/// The `tokens.by_session` rows of one identity as the session list of `recount`'s compare
/// shape (#16).
fn sessions_of(rows: &[bosun_api::SessionRow]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            serde_json::json!({
                "session": row.session,
                "agent": row.agent,
                "messages": row.sums.messages,
                "input": row.sums.input,
                "cache_write_5m": row.sums.cache_write_5m,
                "cache_write_1h": row.sums.cache_write_1h,
                "cache_read": row.sums.cache_read,
                "output": row.sums.output,
            })
        })
        .collect()
}

fn expected(case: &str) -> Value {
    let text = std::fs::read_to_string(fixture(case).join("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn tokens_never_cross_identities() {
    let cases = [
        ("personal", "cache-and-models"),
        ("work", "subagent-cost-state"),
    ];
    let home = Home::new(&cases);
    let _daemon = home.start().await;
    let mut client = home.client().await;

    for (identity, case) in cases {
        let ask = IdentityParams {
            identity: identity.to_owned(),
        };
        // A reingest answers when its pass is done, so the store holds every line after it.
        let reingest = client.call::<method::IngestReingest>(&ask).await.unwrap();
        assert_eq!(reingest.identity, identity);
        assert!(reingest.pass.failed.is_empty(), "{reingest:?}");

        let range = TokensParams {
            identity: identity.to_owned(),
            from_ms: None,
            to_ms: None,
        };
        let expected = expected(case);
        let by_session = client
            .call::<method::TokensBySession>(&range)
            .await
            .unwrap();
        assert_eq!(by_session.summary.identity, identity);
        assert_eq!(
            Value::from(sessions_of(&by_session.rows)),
            expected["sessions"],
            "{identity}: sessions"
        );
        let total = &by_session.summary.total;
        assert_eq!(total.messages, expected["total"]["messages"]);
        assert_eq!(total.output, expected["total"]["output"]);

        for (name, total) in [
            (
                "by_repo",
                client
                    .call::<method::TokensByRepo>(&range)
                    .await
                    .unwrap()
                    .summary
                    .total,
            ),
            (
                "by_model",
                client
                    .call::<method::TokensByModel>(&range)
                    .await
                    .unwrap()
                    .summary
                    .total,
            ),
        ] {
            assert_eq!(total, by_session.summary.total, "{identity}: {name}");
        }

        let status = client.call::<method::IngestStatus>(&ask).await.unwrap();
        assert_eq!(status.identity, identity);
        assert!(status.transcripts.contains(identity), "{status:?}");
        assert!(
            status.files > 0 && status.parsed == status.size,
            "{status:?}"
        );
    }

    let nobody = IdentityParams {
        identity: "nobody".to_owned(),
    };
    match client.call::<method::StatusDaemon>(&nobody).await {
        Err(Error::Rpc(e)) => assert_eq!(e.code, code::UNKNOWN_IDENTITY),
        other => panic!("an unknown identity was served: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn the_socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new(&[("personal", "cache-and-models")]);
    let _daemon = home.start().await;
    let mode = std::fs::metadata(home.endpoint.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
}

/// The producer stage 0's daemon has, end to end: a reingest is published to the identity's
/// subscribers over the endpoint (#69).
#[tokio::test]
async fn a_reingest_reaches_a_subscriber() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    let _daemon = home.start().await;
    let mut subscription =
        Subscription::open(home.endpoint.clone(), "personal", EventFilter::default())
            .await
            .unwrap();
    let ask = IdentityParams {
        identity: "personal".to_owned(),
    };
    let reingest = home
        .client()
        .await
        .call::<method::IngestReingest>(&ask)
        .await
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    let mut last_seq = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let delivery = tokio::time::timeout(left, subscription.next())
            .await
            .expect("no reingest event in time")
            .unwrap();
        let Delivery::Event(event) = delivery else {
            panic!("a resync on a fresh subscription");
        };
        assert!(event.seq > last_seq, "out of order");
        last_seq = event.seq;
        if let EventBody::IngestPass {
            reingest: true,
            pass,
        } = event.body
        {
            assert_eq!(pass.files, reingest.pass.files);
            assert!(pass.files > 0);
            break;
        }
    }

    match Subscription::open(home.endpoint.clone(), "nobody", EventFilter::default()).await {
        Err(Error::Rpc(e)) => assert_eq!(e.code, code::UNKNOWN_IDENTITY),
        other => panic!("an unknown identity was subscribed: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_stale_socket_is_replaced() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    // A socket file nothing listens on, as a daemon that died leaves it.
    drop(std::os::unix::net::UnixListener::bind(home.endpoint.path()).unwrap());
    assert!(home.endpoint.path().exists());
    let daemon = home.start().await;
    assert_eq!(home.client().await.daemon().pid, daemon.pid());
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_cleanly() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    let mut daemon = home.start().await;
    let killed = Command::new("kill")
        .args(["-TERM", &daemon.pid().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = wait(&mut daemon.child)
        .await
        .expect("the daemon did not stop");
    assert_eq!(status.code(), Some(0), "{}", stderr(&mut daemon.child));
    assert!(!home.endpoint.path().exists(), "the socket file is left");
    assert!(
        !home.dir.path().join("operator.token").exists(),
        "the operator token is left"
    );
    let wal = PathBuf::from(format!("{}-wal", home.database("personal").display()));
    let wal_size = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(wal_size, 0, "the WAL was not checkpointed and truncated");
    assert!(matches!(
        Client::connect(&home.endpoint).await,
        Err(Error::NotRunning { .. })
    ));
}

/// #67: the control switches take the operator token the daemon writes at start; a client
/// without it (an agent's own connection) is refused, and `control.status` only reads.
#[tokio::test]
async fn the_switches_take_the_operator_token() {
    use bosun_api::{KillParams, PauseParams};

    let home = Home::new(&[("personal", "cache-and-models")]);
    let _daemon = home.start().await;
    let token = std::fs::read_to_string(home.dir.path().join("operator.token")).unwrap();
    let token = token.trim().to_owned();
    assert!(token.starts_with("bosun-operator-"), "{token}");

    let mut client = home.client().await;
    let refused = |e: Error| matches!(e, Error::Rpc(rpc) if rpc.code == code::UNAUTHORIZED);
    for operator in [String::new(), "bosun-operator-guess".to_owned()] {
        let pause = PauseParams {
            operator: operator.clone(),
            identity: "personal".to_owned(),
            repo: None,
        };
        let e = client
            .call::<method::ControlPause>(&pause)
            .await
            .unwrap_err();
        assert!(refused(e), "pause with {operator:?}");
        let kill = KillParams {
            operator,
            release: false,
        };
        let e = client.call::<method::ControlKill>(&kill).await.unwrap_err();
        assert!(refused(e), "kill");
    }
    let ask = IdentityParams {
        identity: "personal".to_owned(),
    };
    let status = client.call::<method::ControlStatus>(&ask).await.unwrap();
    assert!(!status.paused && !status.killed, "{status:?}");

    let pause = PauseParams {
        operator: token.clone(),
        identity: "personal".to_owned(),
        repo: None,
    };
    assert!(
        client
            .call::<method::ControlPause>(&pause)
            .await
            .unwrap()
            .paused
    );
    let kill = KillParams {
        operator: token,
        release: false,
    };
    assert!(
        client
            .call::<method::ControlKill>(&kill)
            .await
            .unwrap()
            .killed
    );
    let status = client.call::<method::ControlStatus>(&ask).await.unwrap();
    assert!(status.paused && status.killed, "{status:?}");
}
