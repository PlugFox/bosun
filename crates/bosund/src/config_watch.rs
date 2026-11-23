//! Watching the configuration file (#4, #55, #62): an edit is read again and, when valid,
//! replaces the configuration the loops see; an invalid edit keeps the previous one in force
//! and is reported.
//!
//! The file is compared by content every [`INTERVAL`] (a poll, not a file-system watch: an
//! editor's save is often a rename, and a poll survives that on every platform).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_config::{Config, DiskProbe};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the file is read.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

/// The last reload's error, while it is the file's current content.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reported(Arc<Mutex<Option<String>>>);

impl Reported {
    /// The error of the file as it is now, if its last edit was invalid.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the status methods (#70) report it")
    )]
    pub(crate) fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, error: Option<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = error;
    }
}

/// One read of the file's new `text` over `previous`: the configuration now in force, and the
/// error when the edit was invalid (then it is `previous`).
pub(crate) fn reload(
    previous: &Config,
    text: &str,
    probe: &dyn DiskProbe,
) -> (Config, Option<String>) {
    let reload = bosun_config::reload(previous.clone(), text, probe);
    (reload.config, reload.error.map(|e| e.to_string()))
}

/// Watches `path`, publishing each valid edit on `tx`, until the task is aborted.
pub(crate) fn spawn(
    path: PathBuf,
    tx: watch::Sender<Arc<Config>>,
    probe: Arc<dyn DiskProbe + Send + Sync>,
    every: Duration,
    reported: Reported,
) -> JoinHandle<()> {
    // Read now, not in the task: an edit made right after this call is a change.
    let mut last = std::fs::read(&path).ok();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let Ok(now) = tokio::fs::read(&path).await else {
                // Mid-save, or removed: the configuration in force stays.
                continue;
            };
            if last.as_ref() == Some(&now) {
                continue;
            }
            last = Some(now.clone());
            let text = String::from_utf8_lossy(&now);
            let previous = tx.borrow().clone();
            let (config, error) = reload(&previous, &text, probe.as_ref());
            match &error {
                Some(e) => eprintln!(
                    "bosund: {}: the edit is invalid, the previous configuration stays: {e}",
                    path.display()
                ),
                None => eprintln!("bosund: {}: configuration reloaded", path.display()),
            }
            reported.set(error);
            if config != *previous {
                tx.send_replace(Arc::new(config));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(dir: &std::path::Path, protected: &str) -> String {
        let db = dir.join("db").join("bosun.db");
        format!(
            "[identity.test]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\n\
             [repo.demo]\npath = '{}'\nidentity = 'test'\n\n\
             [repo.demo.policy]\nprotected = [{protected}]\n",
            dir.join("claude").display(),
            db.display(),
            dir.join("repo").display(),
        )
    }

    #[tokio::test]
    async fn an_invalid_edit_keeps_the_previous_configuration_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["claude", "db", "repo"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let path = dir.path().join("config.toml");
        let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(bosun_config::SystemProbe::new());
        std::fs::write(&path, text(dir.path(), "'a'")).unwrap();
        let first = bosun_config::load(&path, probe.as_ref()).unwrap();
        let (tx, mut rx) = watch::channel(Arc::new(first.clone()));
        let reported = Reported::default();
        let task = spawn(
            path.clone(),
            tx,
            probe,
            Duration::from_millis(20),
            reported.clone(),
        );

        std::fs::write(&path, "[identity.test\nbroken").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while reported.get().is_none() {
            assert!(std::time::Instant::now() < deadline, "no error reported");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(**rx.borrow(), first, "the previous configuration stays");
        assert!(!rx.has_changed().unwrap());

        std::fs::write(&path, text(dir.path(), "'a', 'b'")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx.changed())
            .await
            .expect("a valid edit is published")
            .unwrap();
        assert!(rx.borrow().repos["demo"].policy.protects("b"));
        assert_eq!(reported.get(), None, "the error clears with the fix");
        task.abort();
    }
}
