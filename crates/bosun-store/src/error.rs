use std::fmt;
use std::path::PathBuf;

/// What can go wrong in the store.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The database was migrated by a newer bosun than this one. It is left untouched.
    NewerDatabase {
        path: PathBuf,
        /// The highest migration version applied to the file.
        found: i64,
        /// The highest migration version this binary knows.
        supported: i64,
    },
    /// A file operation failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlx(sqlx::Error),
    Migrate(sqlx::migrate::MigrateError),
    /// A stored value is not what the schema promises (for example usage JSON that does not
    /// parse, or a negative count).
    Corrupt(String),
    /// A value does not fit its column (a count above `i64::MAX`).
    OutOfRange(&'static str),
    /// A message names a source file that no earlier write registered.
    UnknownSourceFile {
        provider: String,
        file_key: String,
    },
    /// A write names an item that no earlier write registered.
    UnknownItem(String),
    /// A write names an attempt that no recorded transition started.
    UnknownAttempt {
        item: String,
        attempt: u32,
    },
    /// A write ends a session or a verdict run that was not started.
    UnknownRow(String),
    /// Two records with different keys reached the merge.
    Merge(bosun_core::KeyMismatch),
    /// The price rows do not form a valid table.
    Prices(bosun_core::TableError),
    /// A price change that does not follow the current row of its model and threshold.
    PriceChange(String),
    /// The writer task has stopped.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerDatabase {
                path,
                found,
                supported,
            } => write!(
                f,
                "{} is at schema version {found}, newer than this bosun (version {supported}); \
                 it was left untouched: upgrade bosun",
                path.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Sqlx(e) => write!(f, "database: {e}"),
            Self::Migrate(e) => write!(f, "migration: {e}"),
            Self::Corrupt(what) => write!(f, "corrupt database: {what}"),
            Self::OutOfRange(what) => write!(f, "{what} does not fit a database integer"),
            Self::UnknownSourceFile { provider, file_key } => {
                write!(f, "unknown source file {provider}/{file_key}")
            }
            Self::UnknownItem(item) => write!(f, "unknown item {item}"),
            Self::UnknownAttempt { item, attempt } => {
                write!(f, "unknown attempt {attempt} of item {item}")
            }
            Self::UnknownRow(what) => write!(f, "unknown {what}"),
            Self::Merge(e) => e.fmt(f),
            Self::Prices(e) => write!(f, "price table: {e}"),
            Self::PriceChange(why) => write!(f, "price change refused: {why}"),
            Self::Closed => f.write_str("the store is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlx(e) => Some(e),
            Self::Migrate(e) => Some(e),
            Self::Merge(e) => Some(e),
            Self::Prices(e) => Some(e),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl From<sqlx::migrate::MigrateError> for Error {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        Self::Migrate(e)
    }
}

impl From<bosun_core::KeyMismatch> for Error {
    fn from(e: bosun_core::KeyMismatch) -> Self {
        Self::Merge(e)
    }
}

impl From<bosun_core::TableError> for Error {
    fn from(e: bosun_core::TableError) -> Self {
        Self::Prices(e)
    }
}

/// `u64` to a SQLite integer.
pub(crate) fn to_db(value: u64, what: &'static str) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::OutOfRange(what))
}

/// A SQLite integer to `u64`; a negative value is corruption.
pub(crate) fn from_db(value: i64, what: &'static str) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::Corrupt(format!("{what} is negative: {value}")))
}
