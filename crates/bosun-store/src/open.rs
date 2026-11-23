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
