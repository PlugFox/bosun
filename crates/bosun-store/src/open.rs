//! Opening a database: the version check, the pre-migration copy, migrations, pragmas and file
//! permissions (#3, "Migrations and durability").

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::migrate::Migrator;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Connection, SqlitePool};

use crate::error::Error;
use crate::read::Reader;
use crate::writer::{self, Writer};
use crate::{Options, Store};

/// The migrations of this binary, embedded at compile time.
pub(crate) static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

pub(crate) async fn open(
    path: &Path,
    options: &Options,
    migrator: &Migrator,
) -> Result<Store, Error> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(parent)?;
    }

    let exists = fs::exists(path).map_err(io(path))?;
    if exists {
        let applied = applied_versions(path, options).await?;
        let supported = migrator.iter().map(|m| m.version).max().unwrap_or(0);
        if let Some(&found) = applied.last()
            && found > supported
            && !cfg!(mutant_store_accept_newer)
        {
            return Err(Error::NewerDatabase {
                path: path.to_owned(),
                found,
                supported,
            });
        }
        let pending = migrator.iter().any(|m| !applied.contains(&m.version));
        let empty = fs::metadata(path).map_err(io(path))?.len() == 0;
        if pending && !empty {
            copy_before_migration(path, applied.last().copied().unwrap_or(0), options).await?;
        }
        restrict(path)?;
    } else {
        create_private_file(path)?;
    }

    let mut conn = SqliteConnection::connect_with(
        &base_options(path, options)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal),
    )
    .await?;
    migrator.run(&mut conn).await?;
    writer::seed_prices(&mut conn).await?;
    // The file itself holds the migrated schema and the seed, not only the WAL.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut conn)
        .await?;

    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(options.read_connections.max(1))
        .connect_with(base_options(path, options).read_only(true))
        .await?;

    let (writer, task) = Writer::spawn(conn, options.checkpoint_interval);
    Ok(Store {
        writer,
        reader: Reader::new(pool),
        task,
    })
}

/// Options shared by every connection: an existing file (the store creates it, with its
/// permissions, before SQLite sees it), foreign keys on, a busy timeout.
fn base_options(path: &Path, options: &Options) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(options.busy_timeout)
}

/// The migration versions recorded in the file, ascending. Read through a read-only
/// connection that sets no pragma that writes, so a refused file is left as it was.
async fn applied_versions(path: &Path, options: &Options) -> Result<Vec<i64>, Error> {
    let mut conn =
        SqliteConnection::connect_with(&base_options(path, options).read_only(true)).await?;
    let has_table = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!: i64" FROM sqlite_master
           WHERE type = 'table' AND name = '_sqlx_migrations'"#
    )
    .fetch_one(&mut conn)
    .await?;
    let versions = if has_table > 0 {
        sqlx::query_scalar!(
            r#"SELECT version AS "version!: i64" FROM _sqlx_migrations ORDER BY version"#
        )
        .fetch_all(&mut conn)
        .await?
    } else {
        Vec::new()
    };
    conn.close().await?;
    Ok(versions
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// Copies the database, WAL contents included, to `<file>.v<version>-<unix ms>.bak` next to it
/// before a migration runs. `VACUUM INTO` writes a consistent snapshot through a read-only
/// connection; the target is created first so that it gets the store's permissions.
async fn copy_before_migration(path: &Path, version: i64, options: &Options) -> Result<(), Error> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".v{version}-{millis}.bak"));
    let copy = path.with_file_name(name);
    create_private_file(&copy)?;

    let mut conn =
        SqliteConnection::connect_with(&base_options(path, options).read_only(true)).await?;
    let target = copy
        .to_str()
        .ok_or_else(|| Error::Io {
            path: copy.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path is not UTF-8"),
        })?
        .to_owned();
    sqlx::query!("VACUUM INTO ?", target)
        .execute(&mut conn)
        .await?;
    conn.close().await?;
    Ok(())
}

/// Creates a new, empty file readable by the owning user only.
fn create_private_file(path: &Path) -> Result<(), Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if !cfg!(mutant_store_default_perms) {
            options.mode(0o600);
        }
    }
    options.open(path).map(drop).map_err(io(path))
}

/// Creates `dir` and its missing parents; on Unix those it creates are mode 0700.
fn create_private_dir(dir: &Path) -> Result<(), Error> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if !cfg!(mutant_store_default_perms) {
            builder.mode(0o700);
        }
    }
    builder.create(dir).map_err(io(dir))
}

/// Makes an existing database file readable by the owning user only. SQLite gives the WAL and
/// shared-memory files the mode of the database file.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn restrict(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !cfg!(mutant_store_default_perms) {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io(path))?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;

    use sqlx::SqlStr;
    use sqlx::migrate::{Migration, MigrationType};

    use super::*;

    fn store_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("data").join("bosun.db")
    }

    /// This binary's migrations plus one more, as a later bosun would ship them.
    fn later_migrator() -> Migrator {
        let mut migrations = MIGRATOR.migrations.to_vec();
        migrations.push(Migration::new(
            9_000,
            Cow::Borrowed("later"),
            MigrationType::Simple,
            SqlStr::from_static("CREATE TABLE later (id INTEGER PRIMARY KEY) STRICT;"),
            false,
        ));
        Migrator::with_migrations(migrations)
    }

    /// The highest migration version of this binary.
    fn latest() -> i64 {
        MIGRATOR.iter().map(|m| m.version).max().unwrap()
    }

    /// This binary's migrations up to `version`, as an earlier bosun shipped them.
    fn up_to(version: i64) -> Migrator {
        Migrator::with_migrations(
            MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version <= version)
                .cloned()
                .collect(),
        )
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "bak"))
            .collect();
        found.sort();
        found
    }

    /// #46: migration 3 rebuilds `session_check` to make its cost nullable; a check stored by
    /// version 2 survives it, and has no window row, so its window stays open.
    #[tokio::test]
    async fn a_check_of_version_2_survives_migration_3() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(2)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO session (id, provider, session_key) VALUES (7, 'claude', 's');
             INSERT INTO session_check (root_session_id, model, input_tokens, cache_write_tokens,
                                        cache_read_tokens, output_tokens, cost_usd, recorded_ms)
             VALUES (7, 'm', 1, 2, 3, 4, 0.5, 9);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        let row: (i64, String, i64, i64, i64, i64, Option<f64>, i64) = sqlx::query_as(
            "SELECT root_session_id, model, input_tokens, cache_write_tokens, cache_read_tokens,
                    output_tokens, cost_usd, recorded_ms FROM session_check",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(row, (7, "m".to_owned(), 1, 2, 3, 4, Some(0.5), 9));
        let windows: i64 = sqlx::query_scalar("SELECT count(*) FROM session_check_window")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(windows, 0);
        drop(conn);
        store.close().await.unwrap();
    }

    /// #76: migration 4 rebuilds `file_touch` to allow the kinds `exec` and `unknown`; a touch
    /// stored by version 3 survives it, and the new kinds are accepted, others refused.
    #[tokio::test]
    async fn a_touch_of_version_3_survives_migration_4() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let earlier = MIGRATOR
            .migrations
            .iter()
            .filter(|m| m.version <= 3)
            .cloned()
            .collect();
        let store = open(
            &path,
            &Options::default(),
            &Migrator::with_migrations(earlier),
        )
        .await
        .unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO session (id, provider, session_key) VALUES (1, 'claude', 's');
             INSERT INTO source_file (id, provider, file_key, path) VALUES (1, 'claude', 'f', 'p');
             INSERT INTO model (id, provider, name) VALUES (1, 'claude', 'm');
             INSERT INTO message (id, provider, message_key, session_id, source_file_id, ts_ms,
                                  model_id, cwd, input_tokens, cache_write_5m_tokens,
                                  cache_write_1h_tokens, cache_read_tokens, output_tokens,
                                  usage_json, parser_version)
             VALUES (1, 'claude', 'k', 1, 1, 0, 1, '/fixture', 0, 0, 0, 0, 0, '{}', 2);
             INSERT INTO tool_call (id, message_id, tool_use_key, name, input_bytes,
                                    result_bytes, is_error)
             VALUES (1, 1, 't', 'Read', 0, 0, 0);
             INSERT INTO file_touch (id, tool_call_id, path, kind) VALUES (5, 1, 'a.rs', 'read');",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        let row: (i64, i64, String, String) =
            sqlx::query_as("SELECT id, tool_call_id, path, kind FROM file_touch")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(row, (5, 1, "a.rs".to_owned(), "read".to_owned()));
        drop(conn);
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        for kind in ["exec", "unknown"] {
            sqlx::query("INSERT INTO file_touch (tool_call_id, path, kind) VALUES (1, 'b.py', ?)")
                .bind(kind)
                .execute(&mut conn)
                .await
                .unwrap();
        }
        let refused =
            sqlx::query("INSERT INTO file_touch (tool_call_id, path, kind) VALUES (1, 'c', 'x')")
                .execute(&mut conn)
                .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();
    }

    /// #95: migration 7 backfills the span of every window that owns messages, sub-agents
    /// included, from its first and last message; a window without messages keeps none.
    #[tokio::test]
    async fn migration_7_backfills_the_session_spans() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(6)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO source_file (id, provider, file_key, path) VALUES (1, 'claude', 'f', 'p');
             INSERT INTO model (id, provider, name) VALUES (1, 'claude', 'm');
             INSERT INTO session (id, provider, session_key) VALUES (1, 'claude', 's1');
             INSERT INTO session (id, provider, session_key, agent_key, parent_id, root_id)
                 VALUES (2, 'claude', 's1', 'agent-1', 1, 1);
             INSERT INTO session (id, provider, session_key) VALUES (3, 'claude', 'empty');
             INSERT INTO message (provider, message_key, session_id, source_file_id, ts_ms,
                                  model_id, cwd, input_tokens, cache_write_5m_tokens,
                                  cache_write_1h_tokens, cache_read_tokens, output_tokens,
                                  usage_json, parser_version)
             VALUES ('claude', 'm1', 1, 1, 200, 1, '/w', 0, 0, 0, 0, 0, '{}', 2),
                    ('claude', 'm2', 1, 1, 100, 1, '/w', 0, 0, 0, 0, 0, '{}', 2),
                    ('claude', 'm3', 1, 1, 900, 1, '/w', 0, 0, 0, 0, 0, '{}', 2),
                    ('claude', 'm4', 2, 1, 400, 1, '/w', 0, 0, 0, 0, 0, '{}', 2);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let spans: Vec<(i64, Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT id, started_ms, ended_ms FROM session ORDER BY id")
                .fetch_all(store.reader().pool())
                .await
                .unwrap();
        assert_eq!(
            spans,
            [
                (1, Some(100), Some(900)),
                (2, Some(400), Some(400)),
                (3, None, None)
            ]
        );
        store.close().await.unwrap();
    }

    /// #64: migration 8 rebuilds `item` to allow the reason `handshake`; an item stored by
    /// version 7, with an attempt and a transition referring to it, survives it, the foreign
    /// keys still hold, and the new reason is accepted, an unknown one refused.
    #[tokio::test]
    async fn an_item_of_version_7_survives_migration_8() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(7)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/repo');
             INSERT INTO item (id, item_key, repo_id, state, reason, attempt_number, seq,
                               created_ms, updated_ms)
             VALUES (3, 'demo#1', 1, 'needs_you', 'auth', 1, 4, 10, 20);
             INSERT INTO attempt (id, item_id, number, state, spawn, started_ms)
             VALUES (1, 3, 1, 'exited', 1, 10);
             INSERT INTO transition (transition_key, item_id, seq, attempt, from_state, to_state,
                                     cause, ts_ms)
             VALUES ('demo#1/t4', 3, 4, 1, 'running', 'needs_you', 'exited', 20);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let pool = store.reader().pool();
        let row: (i64, String, String, Option<String>, i64) =
            sqlx::query_as("SELECT id, item_key, state, reason, seq FROM item")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            row,
            (
                3,
                "demo#1".to_owned(),
                "needs_you".to_owned(),
                Some("auth".to_owned()),
                4
            )
        );
        let broken: Vec<(String, i64)> =
            sqlx::query_as("SELECT \"table\", rowid FROM pragma_foreign_key_check")
                .fetch_all(pool)
                .await
                .unwrap();
        assert!(broken.is_empty(), "{broken:?}");
        let indexes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index'
               AND name IN ('item_repo', 'item_state')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(indexes, 2);
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE item SET reason = 'handshake' WHERE id = 3")
            .execute(&mut conn)
            .await
            .unwrap();
        let refused = sqlx::query("UPDATE item SET reason = 'unknown' WHERE id = 3")
            .execute(&mut conn)
            .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();
    }

    /// #99: migration 9 rebuilds `item` to allow the reason `identity` and gives `attempt` its
    /// guard snapshots; an item of version 8 with an attempt survives it, the foreign keys still
    /// hold, and the attempt's guards are written and read back.
    #[tokio::test]
    async fn an_item_of_version_8_survives_migration_9_and_its_attempt_takes_guards() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(8)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/repo');
             INSERT INTO item (id, item_key, repo_id, state, reason, attempt_number, seq,
                               created_ms, updated_ms)
             VALUES (3, 'demo#1', 1, 'needs_you', 'handshake', 1, 4, 10, 20);
             INSERT INTO attempt (id, item_id, number, state, spawn, started_ms)
             VALUES (1, 3, 1, 'exited', 1, 10);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let pool = store.reader().pool();
        let broken: Vec<(String, i64)> =
            sqlx::query_as("SELECT \"table\", rowid FROM pragma_foreign_key_check")
                .fetch_all(pool)
                .await
                .unwrap();
        assert!(broken.is_empty(), "{broken:?}");
        let id = bosun_core::ItemId::new("demo#1");
        assert_eq!(store.reader().attempt_guards(&id, 1).await.unwrap(), None);
        store
            .writer()
            .write(vec![crate::Write::AttemptGuards {
                item: id.clone(),
                attempt: 1,
                ignored_json: "[\"x.log\"]".to_owned(),
                git_config_json: "{}".to_owned(),
            }])
            .await
            .unwrap();
        assert_eq!(
            store.reader().attempt_guards(&id, 1).await.unwrap(),
            Some(("[\"x.log\"]".to_owned(), "{}".to_owned()))
        );
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE item SET reason = 'identity' WHERE id = 3")
            .execute(&mut conn)
            .await
            .unwrap();
        let refused = sqlx::query("UPDATE item SET reason = 'unknown' WHERE id = 3")
            .execute(&mut conn)
            .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();
    }

    /// #150: migration 13 rebuilds `item` to add `conflicts` and the reasons `setup` and
    /// `guard_checkout`, and adds stage 2's tables. A stage-1 database (version 12) with an
    /// item that needs the user and a done one migrates: both read back unchanged with no
    /// conflicts, the foreign keys hold, and the new reason is accepted, an unknown one refused.
    #[tokio::test]
    async fn a_stage_1_database_migrates_to_stage_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(12)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/repo');
             INSERT INTO item (id, item_key, repo_id, card_key, state, reason, attempt_number,
                               seq, attempts, spawns, runs, failed, created_ms, updated_ms)
             VALUES (3, 'demo#1', 1, 'card-1', 'needs_you', 'identity', 1, 4, 1, 1, 0, 0, 10, 20);
             INSERT INTO item (id, item_key, repo_id, state, outcome, attempt_number, seq,
                               attempts, spawns, runs, boot, created_ms, updated_ms)
             VALUES (8, 'demo#2', 1, 'done', 'flaky', 2, 9, 2, 3, 2, 1, 30, 40);
             INSERT INTO attempt (id, item_id, number, state, spawn, started_ms)
             VALUES (1, 3, 1, 'exited', 1, 10);
             INSERT INTO transition (transition_key, item_id, seq, attempt, from_state, to_state,
                                     cause, ts_ms)
             VALUES ('demo#1/t4', 3, 4, 1, 'running', 'needs_you', 'exited', 20);
             INSERT INTO item_note (item_id, kind, body, ts_ms) VALUES (8, 'lint', '[]', 35);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let columns = "SELECT id, item_key, repo_id, card_key, state, park_reason, park_until_ms,
                              park_next, outcome, reason, attempt_number, snapshot_id, seq,
                              attempts, spawns, runs, failed, uncounted, boot, created_ms,
                              updated_ms
                       FROM item ORDER BY id";
        let before = items(&mut conn, columns).await;
        assert_eq!(before.len(), 2);
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        assert_eq!(items(&mut conn, columns).await, before);
        let conflicts: Vec<i64> = sqlx::query_scalar("SELECT conflicts FROM item ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
        assert_eq!(conflicts, [0, 0]);
        let broken: Vec<(String, i64)> =
            sqlx::query_as("SELECT \"table\", rowid FROM pragma_foreign_key_check")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert!(broken.is_empty(), "{broken:?}");
        let found: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master
             WHERE name IN ('item_repo', 'item_state', 'worktree', 'worktree_item', 'process',
                            'process_item', 'lease_wait', 'landing')
             ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            found,
            [
                "item_repo",
                "item_state",
                "landing",
                "lease_wait",
                "process",
                "process_item",
                "worktree",
                "worktree_item"
            ]
        );
        drop(conn);
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        for reason in ["setup", "guard_checkout"] {
            sqlx::query(
                "INSERT INTO item (item_key, repo_id, state, reason, created_ms, updated_ms)
                 VALUES (?, 1, 'needs_you', ?, 50, 50)",
            )
            .bind(format!("demo#{reason}"))
            .bind(reason)
            .execute(&mut conn)
            .await
            .unwrap();
        }
        let refused = sqlx::query(
            "INSERT INTO item (item_key, repo_id, state, reason, created_ms, updated_ms)
             VALUES ('demo#x', 1, 'needs_you', 'nonsense', 50, 50)",
        )
        .execute(&mut conn)
        .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();
    }

    /// Migration 13 runs outside sqlx's transaction, so a daemon that dies after its `COMMIT`
    /// and before sqlx records it leaves its tables without its record. The next open runs it
    /// again: it must not stop at a table it made the first time, and every row survives.
    #[tokio::test]
    async fn migration_13_runs_again_when_its_record_was_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = Store::open(&path, Options::default()).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/repo');
             INSERT INTO item (id, item_key, repo_id, card_key, state, reason, attempt_number,
                               seq, attempts, spawns, runs, failed, created_ms, updated_ms)
             VALUES (3, 'demo#1', 1, 'card-1', 'needs_you', 'setup', 1, 4, 1, 1, 0, 0, 10, 20);
             INSERT INTO worktree (id, item_id, path, branch, base_sha, kind, user_base_json,
                                   created_ms)
             VALUES (5, 3, '/fixture/worktrees/demo-1', 'bosun/demo-1', 'abc123', 'item',
                     '{}', 15);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let item_columns = "SELECT id, item_key, repo_id, card_key, state, park_reason,
                                   park_until_ms, park_next, outcome, reason, attempt_number,
                                   snapshot_id, seq, attempts, spawns, runs, failed, uncounted,
                                   boot, created_ms, updated_ms, conflicts
                            FROM item ORDER BY id";
        let worktree_columns = "SELECT id, item_id, path, branch, base_sha, kind, state, blocking,
                                       error, bytes, user_base_json, created_ms, ended_ms
                                FROM worktree ORDER BY id";
        let items_before = items(&mut conn, item_columns).await;
        let worktrees_before = items(&mut conn, worktree_columns).await;
        assert_eq!(items_before.len(), 1);
        assert_eq!(worktrees_before.len(), 1);
        // As a crash between the migration's `COMMIT` and sqlx's insert leaves it.
        sqlx::raw_sql("DELETE FROM _sqlx_migrations WHERE version = 13")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        assert_eq!(items(&mut conn, item_columns).await, items_before);
        assert_eq!(items(&mut conn, worktree_columns).await, worktrees_before);
        let recorded: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 13")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(recorded, 1);
        let broken: Vec<(String, i64)> =
            sqlx::query_as("SELECT \"table\", rowid FROM pragma_foreign_key_check")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert!(broken.is_empty(), "{broken:?}");
        drop(conn);
        store.close().await.unwrap();
    }

    /// #132: migration 16 names each repository row after the prefix of its items' ids
    /// (`<repo name>#<card>`), leaves a row without items unnamed, and adds an empty
    /// `repo_alias`.
    #[tokio::test]
    async fn migration_16_names_each_repository_after_its_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(15)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/demo'), (2, '/fixture/other'),
                                                (3, '/fixture/plain');
             INSERT INTO item (item_key, repo_id, state, created_ms, updated_ms)
             VALUES ('demo#1', 1, 'ready', 10, 20), ('demo#2', 1, 'ready', 10, 30),
                    ('other#5', 2, 'ready', 10, 25);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        let names: Vec<(i64, Option<String>)> =
            sqlx::query_as("SELECT id, name FROM repo ORDER BY id")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            names,
            [
                (1, Some("demo".to_owned())),
                (2, Some("other".to_owned())),
                (3, None)
            ]
        );
        let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_alias")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(aliases, 0);
        drop(conn);
        store.close().await.unwrap();
    }

    /// A name whose items sit on two rows (a move before migration 16) goes to the row of the
    /// most recently updated item; the other row stays unnamed.
    #[tokio::test]
    async fn migration_16_gives_a_split_name_to_the_row_of_its_latest_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(15)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path) VALUES (1, '/fixture/old'), (2, '/fixture/new');
             INSERT INTO item (item_key, repo_id, state, created_ms, updated_ms)
             VALUES ('demo#1', 1, 'ready', 10, 20), ('demo#2', 2, 'ready', 10, 40),
                    ('demo#3', 1, 'ready', 10, 30);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let mut conn = store.reader().pool.acquire().await.unwrap();
        let names: Vec<(i64, Option<String>)> =
            sqlx::query_as("SELECT id, name FROM repo ORDER BY id")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert_eq!(names, [(1, None), (2, Some("demo".to_owned()))]);
        drop(conn);
        store.close().await.unwrap();
    }

    /// #132: migration 17 rebuilds `item` to allow the reason `stale`. A database of version 16
    /// with an item that needs the user and its attempt migrates: both read back the same, the
    /// foreign keys hold, the new reason is accepted, an unknown one refused; and the migration
    /// runs again cleanly when its record was lost (it runs outside sqlx's transaction).
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn migration_17_accepts_the_stale_reason_and_keeps_every_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(16)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path, name) VALUES (1, '/fixture/demo', 'demo');
             INSERT INTO item (id, item_key, repo_id, card_key, state, reason, attempt_number,
                               seq, attempts, spawns, runs, failed, uncounted, boot, created_ms,
                               updated_ms, conflicts)
             VALUES (3, 'demo#1', 1, 'card-1', 'needs_you', 'lint', 1, 4, 1, 2, 1, 0, 1, 2, 10,
                     20, 1);
             INSERT INTO attempt (id, item_id, number, state, spawn, started_ms)
             VALUES (1, 3, 1, 'exited', 1, 10);
             INSERT INTO transition (transition_key, item_id, seq, attempt, from_state, to_state,
                                     cause, ts_ms)
             VALUES ('demo#1/t4', 3, 4, 1, 'ready', 'needs_you', 'lint_failed', 20);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let item_columns = "SELECT id, item_key, repo_id, card_key, state, park_reason,
                                   park_until_ms, park_next, outcome, reason, attempt_number,
                                   snapshot_id, seq, attempts, spawns, runs, failed, uncounted,
                                   boot, created_ms, updated_ms, conflicts
                            FROM item ORDER BY id";
        let attempt_columns =
            "SELECT id, item_id, number, state, spawn, started_ms FROM attempt ORDER BY id";
        let items_before = items(&mut conn, item_columns).await;
        let attempts_before = items(&mut conn, attempt_columns).await;
        assert_eq!(items_before.len(), 1);
        assert_eq!(attempts_before.len(), 1);
        conn.close().await.unwrap();

        for pass in 0..2 {
            let store = Store::open(&path, Options::default()).await.unwrap();
            let mut conn = store.reader().pool.acquire().await.unwrap();
            assert_eq!(items(&mut conn, item_columns).await, items_before, "{pass}");
            assert_eq!(
                items(&mut conn, attempt_columns).await,
                attempts_before,
                "{pass}"
            );
            let broken: Vec<(String, i64)> =
                sqlx::query_as("SELECT \"table\", rowid FROM pragma_foreign_key_check")
                    .fetch_all(&mut *conn)
                    .await
                    .unwrap();
            assert!(broken.is_empty(), "{broken:?}");
            let indexes: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index'
                   AND name IN ('item_repo', 'item_state')",
            )
            .fetch_one(&mut *conn)
            .await
            .unwrap();
            assert_eq!(indexes, 2);
            drop(conn);
            store.close().await.unwrap();
            if pass == 0 {
                // As a crash between the migration's `COMMIT` and sqlx's insert leaves it.
                let mut conn =
                    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                        .await
                        .unwrap();
                sqlx::raw_sql("DELETE FROM _sqlx_migrations WHERE version = 17")
                    .execute(&mut conn)
                    .await
                    .unwrap();
                conn.close().await.unwrap();
            }
        }

        let store = Store::open(&path, Options::default()).await.unwrap();
        let id = bosun_core::ItemId::new("demo#1");
        let stored = store.reader().item(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.item.state,
            bosun_core::State::NeedsYou(bosun_core::Reason::Lint)
        );
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE item SET reason = 'stale' WHERE id = 3")
            .execute(&mut conn)
            .await
            .unwrap();
        let refused = sqlx::query("UPDATE item SET reason = 'unknown' WHERE id = 3")
            .execute(&mut conn)
            .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let stored = store.reader().item(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.item.state,
            bosun_core::State::NeedsYou(bosun_core::Reason::Stale)
        );
        store.close().await.unwrap();
    }

    /// #7: migration 21 rebuilds `item` to allow the reason `census_drop`. An item of a database
    /// of version 19 reads back the same; the new reason is accepted and read back, an unknown
    /// one refused.
    #[tokio::test]
    async fn migration_21_accepts_the_census_drop_reason_and_keeps_every_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = open(&path, &Options::default(), &up_to(19)).await.unwrap();
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repo (id, path, name) VALUES (1, '/fixture/demo', 'demo');
             INSERT INTO item (id, item_key, repo_id, card_key, state, reason, attempt_number,
                               seq, attempts, spawns, runs, failed, uncounted, boot, created_ms,
                               updated_ms, conflicts)
             VALUES (3, 'demo#1', 1, 'card-1', 'needs_you', 'stale', 1, 4, 1, 2, 1, 0, 1, 2, 10,
                     20, 1);
             INSERT INTO attempt (id, item_id, number, state, spawn, started_ms)
             VALUES (1, 3, 1, 'exited', 1, 10);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let item_columns = "SELECT id, item_key, repo_id, card_key, state, park_reason,
                                   park_until_ms, park_next, outcome, reason, attempt_number,
                                   snapshot_id, seq, attempts, spawns, runs, failed, uncounted,
                                   boot, created_ms, updated_ms, conflicts
                            FROM item ORDER BY id";
        let before = items(&mut conn, item_columns).await;
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        {
            let mut conn = store.reader().pool.acquire().await.unwrap();
            assert_eq!(items(&mut conn, item_columns).await, before);
        }
        store.close().await.unwrap();

        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE item SET reason = 'census_drop' WHERE id = 3")
            .execute(&mut conn)
            .await
            .unwrap();
        let refused = sqlx::query("UPDATE item SET reason = 'unknown' WHERE id = 3")
            .execute(&mut conn)
            .await;
        assert!(refused.is_err());
        conn.close().await.unwrap();

        let store = Store::open(&path, Options::default()).await.unwrap();
        let id = bosun_core::ItemId::new("demo#1");
        let stored = store.reader().item(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.item.state,
            bosun_core::State::NeedsYou(bosun_core::Reason::CensusDrop)
        );
        store.close().await.unwrap();
    }

    /// Every row of `query` as text, column by column, to compare a table before and after.
    async fn items(conn: &mut SqliteConnection, query: &'static str) -> Vec<Vec<Option<String>>> {
        use sqlx::Row;
        sqlx::query(query)
            .fetch_all(conn)
            .await
            .unwrap()
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| {
                        row.try_get::<Option<i64>, _>(i)
                            .map(|v| v.map(|v| v.to_string()))
                            .or_else(|_| row.try_get::<Option<String>, _>(i))
                            .unwrap()
                    })
                    .collect()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_newer_database_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let later = later_migrator();
        let store = open(&path, &Options::default(), &later).await.unwrap();
        store.close().await.unwrap();

        let before = fs::read(&path).unwrap();
        let err = Store::open(&path, Options::default()).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::NewerDatabase {
                    found: 9_000,
                    supported,
                    ..
                } if supported == latest()
            ),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "the refused file changed");
        assert_eq!(backups(path.parent().unwrap()), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn a_migration_leaves_a_copy_of_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = Store::open(&path, Options::default()).await.unwrap();
        store
            .writer()
            .write(vec![crate::writer::tests::source_file("f1")])
            .await
            .unwrap();
        store.close().await.unwrap();
        // Opening at the same version migrates nothing and copies nothing.
        Store::open(&path, Options::default())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        assert_eq!(backups(path.parent().unwrap()), Vec::<PathBuf>::new());

        let store = open(&path, &Options::default(), &later_migrator())
            .await
            .unwrap();
        store.close().await.unwrap();

        let copies = backups(path.parent().unwrap());
        assert_eq!(copies.len(), 1, "{copies:?}");
        let name = copies[0].file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(&format!("bosun.db.v{}-", latest())),
            "{name}"
        );
        // The copy is the file as it was: this version, the data, no table of the later one.
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&copies[0])
                .read_only(true),
        )
        .await
        .unwrap();
        let versions =
            sqlx::query_scalar!(r#"SELECT version AS "version!: i64" FROM _sqlx_migrations"#)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            versions,
            MIGRATOR.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        let files = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM source_file"#)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(files, 1);
        let later_tables = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!: i64" FROM sqlite_master WHERE name = 'later'"#
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(later_tables, 0);
    }

    /// #57: a stage-0 database (migrations 1 to 3) migrates to this binary's schema, leaving a
    /// copy of itself, and its bill is unchanged.
    #[tokio::test]
    async fn a_stage_0_database_migrates_with_a_copy_and_the_same_bill() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        // The rows a stage-0 bosun wrote: this binary's writer needs the later schema.
        let store = open(&path, &Options::default(), &up_to(3)).await.unwrap();
        let before = store.reader().bill(0, i64::MAX).await.unwrap();
        assert_eq!(before.messages, 0);
        store.close().await.unwrap();
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO source_file (id, provider, file_key, path) VALUES (1, 'claude', 'f1', 'p');
             INSERT INTO session (id, provider, session_key) VALUES (1, 'claude', 's1');
             INSERT INTO session (id, provider, session_key, agent_key, parent_id, root_id)
                 VALUES (2, 'claude', 's1', 'agent-1', 1, 1);
             INSERT INTO model (id, provider, name) VALUES
                 (1, 'claude', 'claude-opus-5-5'), (2, 'claude', 'claude-haiku-5-5');
             INSERT INTO message (provider, message_key, session_id, source_file_id, ts_ms,
                                  model_id, cwd, input_tokens, cache_write_5m_tokens,
                                  cache_write_1h_tokens, cache_read_tokens, output_tokens,
                                  usage_json, parser_version)
             VALUES ('claude', 'm1', 1, 1, 100, 1, '/w', 10, 1000, 2000, 50000, 80, '{}', 1),
                    ('claude', 'm2', 1, 1, 200, 1, '/w', 3, 0, 500, 9000, 70, '{}', 1),
                    ('claude', 'm3', 2, 1, 300, 2, '/w', 7, 100, 0, 4000, 9, '{}', 1);",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        let store = open(&path, &Options::default(), &up_to(3)).await.unwrap();
        let before = store.reader().bill(0, i64::MAX).await.unwrap();
        store.close().await.unwrap();
        assert_eq!(before.messages, 3);
        assert!(before.unpriced.is_empty(), "{before:?}");

        let store = Store::open(&path, Options::default()).await.unwrap();
        let after = store.reader().bill(0, i64::MAX).await.unwrap();
        assert_eq!(after, before);
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table'
             AND name IN ('item', 'transition', 'outbox') ORDER BY name",
        )
        .fetch_all(store.reader().pool())
        .await
        .unwrap();
        assert_eq!(tables, ["item", "outbox", "transition"]);
        store.close().await.unwrap();

        let copies = backups(path.parent().unwrap());
        assert_eq!(copies.len(), 1, "{copies:?}");
        let name = copies[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("bosun.db.v3-"), "{name}");
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&copies[0])
                .read_only(true),
        )
        .await
        .unwrap();
        let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM message")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(messages, 3);
        let loop_tables: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name = 'item'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(loop_tables, 0, "the copy is the stage-0 file");
        conn.close().await.unwrap();
    }

    /// #9: migration 20 came after 21 was shipped; a database migrated to 21 without it gets it
    /// at the next open, with a copy first.
    #[tokio::test]
    async fn a_database_at_21_without_20_gets_the_channel_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let without = Migrator::with_migrations(
            MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version != 20)
                .cloned()
                .collect(),
        );
        open(&path, &Options::default(), &without)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        let store = open(&path, &Options::default(), &MIGRATOR).await.unwrap();
        assert_eq!(
            store.reader().channels().await.unwrap(),
            Vec::<crate::ChannelRow>::new()
        );
        store.close().await.unwrap();
        assert_eq!(backups(path.parent().unwrap()).len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn database_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let store = Store::open(&path, Options::default()).await.unwrap();
        store
            .writer()
            .write(vec![crate::writer::tests::source_file("f1")])
            .await
            .unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700, "the directory");
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            assert_eq!(mode(Path::new(&name)), 0o600, "bosun.db{suffix}");
        }
        store.close().await.unwrap();

        // A file that lost its mode, whatever the umask: opening restores it, and a migration
        // copy is private too.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let store = open(&path, &Options::default(), &later_migrator())
            .await
            .unwrap();
        assert_eq!(mode(&path), 0o600, "an existing file");
        let copies = backups(path.parent().unwrap());
        assert_eq!(copies.len(), 1);
        assert_eq!(mode(&copies[0]), 0o600, "the pre-migration copy");
        store.close().await.unwrap();
    }
}
