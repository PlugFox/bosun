//! The credential store of the channels' secrets (#9, the decision on channels; #4: "the file
//! names credential-store entries").
//!
//! A secret is entered once and kept by the OS: Windows Credential Manager, the macOS Keychain,
//! or the Secret Service on Linux and the other Unixes ([`OsSecrets`]). Every entry is filed
//! under the service [`crate::SERVICE`] with the user [`crate::entry`]
//! (`notify/<identity>/<channel>/<id>`, the id random per row), and that user string is all the
//! channel's row keeps; a row written before the id keeps `notify/<identity>/<channel>`, read as
//! it is.
//! [`MemorySecrets`] stands in for the OS store in tests, so no test of the daemon or the CLI
//! reaches it.
//!
//! On Windows an entry is written with `Local` persistence: it stays on this machine and does
//! not roam with the profile.
//!
//! Errors name the entry and what the store said, never the secret.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use crate::{SECRET_CAP, SERVICE};

/// What went wrong in a credential store. It never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError(pub String);

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretError {}

/// A credential store, by entry name.
pub trait Secrets: Send + Sync + fmt::Debug {
    /// Stores `secret` under `entry`, replacing what was there.
    ///
    /// # Errors
    ///
    /// The secret is longer than [`SECRET_CAP`] UTF-16 units or empty, or the store refused it.
    fn put(&self, entry: &str, secret: &str) -> Result<(), SecretError>;

    /// The secret under `entry`, `None` when there is none.
    ///
    /// # Errors
    ///
    /// The store could not be read.
    fn get(&self, entry: &str) -> Result<Option<String>, SecretError>;

    /// Deletes `entry`; deleting an absent entry is not an error.
    ///
    /// # Errors
    ///
    /// The store refused.
    fn delete(&self, entry: &str) -> Result<(), SecretError>;
}

/// Refuses a secret no store should take.
fn check(entry: &str, secret: &str) -> Result<(), SecretError> {
    if secret.is_empty() {
        return Err(SecretError(format!("{entry}: the secret is empty")));
    }
    if crate::secret_too_long(secret) {
        return Err(SecretError(format!(
            "{entry}: the secret is longer than {SECRET_CAP} UTF-16 units"
        )));
    }
    Ok(())
}

/// A credential store in memory, for tests. Its `Debug` names the entries only, so a failing
/// assertion that prints it prints no secret.
#[derive(Default)]
pub struct MemorySecrets {
    entries: Mutex<BTreeMap<String, String>>,
}

impl fmt::Debug for MemorySecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecrets")
            .field("entries", &self.entries())
            .finish()
    }
}

impl MemorySecrets {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The names of every entry held, in order.
    #[must_use]
    pub fn entries(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Secrets for MemorySecrets {
    fn put(&self, entry: &str, secret: &str) -> Result<(), SecretError> {
        check(entry, secret)?;
        self.lock().insert(entry.to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(&self, entry: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().get(entry).cloned())
    }

    fn delete(&self, entry: &str) -> Result<(), SecretError> {
        self.lock().remove(entry);
        Ok(())
    }
}

/// The OS credential store. Each call builds its entry from the platform store object, never
/// through keyring-core's process-wide default store.
#[derive(Debug, Default)]
pub struct OsSecrets {
    /// Another service than [`SERVICE`]: the round-trip test's own, so it never meets a real
    /// entry.
    pub(crate) service: Option<String>,
}

impl OsSecrets {
    /// The store of this OS, under [`SERVICE`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn service(&self) -> &str {
        self.service.as_deref().unwrap_or(SERVICE)
    }

    fn entry(&self, entry: &str) -> Result<keyring_core::Entry, SecretError> {
        platform::entry(self.service(), entry).map_err(|e| failed(entry, &e))
    }
}

/// What the store said, without anything it may have attached of the secret.
fn failed(entry: &str, error: &keyring_core::Error) -> SecretError {
    use keyring_core::Error as E;
    let said = match error {
        E::BadEncoding(_) => "the stored secret is not UTF-8".to_owned(),
        E::BadDataFormat(..) => "the stored secret is malformed".to_owned(),
        E::Ambiguous(found) => format!("{} entries match", found.len()),
        other => other.to_string(),
    };
    SecretError(format!("the credential store, entry {entry}: {said}"))
}

impl Secrets for OsSecrets {
    fn put(&self, entry: &str, secret: &str) -> Result<(), SecretError> {
        check(entry, secret)?;
        self.entry(entry)?
            .set_password(secret)
            .map_err(|e| failed(entry, &e))
    }

    fn get(&self, entry: &str) -> Result<Option<String>, SecretError> {
        match self.entry(entry)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(failed(entry, &e)),
        }
    }

    fn delete(&self, entry: &str) -> Result<(), SecretError> {
        match self.entry(entry)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(failed(entry, &e)),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;

    use keyring_core::api::CredentialStoreApi;

    pub(super) fn entry(service: &str, user: &str) -> keyring_core::Result<keyring_core::Entry> {
        let store = windows_native_keyring_store::Store::new()?;
        // `Local`: the entry stays on this machine and does not roam with the profile.
        let modifiers = HashMap::from([("persistence", "Local")]);
        store.build(service, user, Some(&modifiers))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use keyring_core::api::CredentialStoreApi;

    pub(super) fn entry(service: &str, user: &str) -> keyring_core::Result<keyring_core::Entry> {
        let store = apple_native_keyring_store::keychain::Store::new()?;
        store.build(service, user, None)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use keyring_core::api::CredentialStoreApi;

    pub(super) fn entry(service: &str, user: &str) -> keyring_core::Result<keyring_core::Entry> {
        let store = zbus_secret_service_keyring_store::Store::new()?;
        store.build(service, user, None)
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    pub(super) fn entry(_: &str, _: &str) -> keyring_core::Result<keyring_core::Entry> {
        Err(keyring_core::Error::NotSupportedByStore(
            "this platform has no credential store bosun knows".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_keeps_and_deletes_entries() {
        let store = MemorySecrets::new();
        store
            .put("notify/a/ops", "synthetic-secret-4f1c9a")
            .unwrap();
        assert_eq!(
            store.get("notify/a/ops").unwrap().as_deref(),
            Some("synthetic-secret-4f1c9a")
        );
        assert_eq!(store.get("notify/b/ops").unwrap(), None);
        store.delete("notify/a/ops").unwrap();
        assert_eq!(store.get("notify/a/ops").unwrap(), None);
        store.delete("notify/a/ops").unwrap();
        assert!(store.put("notify/a/ops", "").is_err());
        let long = "x".repeat(SECRET_CAP + 1);
        let refused = store.put("notify/a/ops", &long).unwrap_err();
        assert!(!refused.0.contains(&long));
        assert_eq!(store.entries(), Vec::<String>::new());
    }

    #[test]
    fn memory_secrets_debug_shows_names_only() {
        let store = MemorySecrets::new();
        store
            .put("notify/a/ops/0f", "synthetic-secret-4f1c9a")
            .unwrap();
        let shown = format!("{store:?} {store:#?}");
        assert!(shown.contains("notify/a/ops/0f"), "{shown}");
        assert!(!shown.contains("synthetic-secret-4f1c9a"), "{shown}");
    }

    /// Deletes its entry when dropped, so the entry is gone also when an assertion panics.
    #[cfg(windows)]
    struct Guard<'a> {
        store: &'a OsSecrets,
        entry: &'a str,
    }

    #[cfg(windows)]
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            let _ = self.store.delete(self.entry);
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_credential_store_round_trips_an_entry() {
        use std::hash::{BuildHasher, RandomState};

        let random = RandomState::new().hash_one(std::time::SystemTime::now());
        let store = OsSecrets {
            service: Some(format!("bosun-test-{}-{random:016x}", std::process::id())),
        };
        let entry = "notify/test/round-trip";
        let _guard = Guard {
            store: &store,
            entry,
        };
        assert_eq!(store.get(entry).unwrap(), None);
        store.put(entry, "synthetic-secret-4f1c9a").unwrap();
        assert_eq!(
            store.get(entry).unwrap().as_deref(),
            Some("synthetic-secret-4f1c9a")
        );
        store.put(entry, "synthetic-secret-77e0b2").unwrap();
        assert_eq!(
            store.get(entry).unwrap().as_deref(),
            Some("synthetic-secret-77e0b2")
        );
        store.delete(entry).unwrap();
        assert_eq!(store.get(entry).unwrap(), None);
        store.delete(entry).unwrap();
    }
}
