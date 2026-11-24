//! Cross-checks of a draft: identity isolation (#2), the local-disk rule for databases and the
//! budget's reserve.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::disk::{Disk, DiskProbe};
use crate::error::Problem;
use crate::read::{Draft, DraftIdentity, PathValue};

/// A path of an identity bundle, read from a draft identity.
type BundlePath = fn(&DraftIdentity) -> Option<&PathValue>;

/// The paths of an identity bundle that no two identities share (#2), each with its name.
const BUNDLE_PATHS: [(&str, BundlePath); 6] = [
    ("Claude configuration directory", |i| {
        i.claude_config_dir.as_ref()
    }),
    ("database file", |i| i.database.as_ref()),
    ("gh configuration directory", |i| i.gh_config_dir.as_ref()),
    ("git configuration file", |i| i.git_config.as_ref()),
    ("SSH key", |i| i.ssh_key.as_ref()),
    ("Codex home", |i| i.codex_home.as_ref()),
];

/// Appends every problem of `draft` to `problems`.
pub(crate) fn validate(draft: &Draft, probe: &dyn DiskProbe, problems: &mut Vec<Problem>) {
    let names: BTreeSet<&str> = draft.identities.iter().map(|i| i.name.as_str()).collect();

    for repo in &draft.repos {
        if let Some((identity, at)) = &repo.identity
            && !names.contains(identity.as_str())
        {
            problems.push(Problem {
                location: *at,
                message: format!(
                    "`repo.{}` binds identity `{identity}`, which is not configured",
                    repo.name
                ),
            });
        }
    }
    distinct(
        draft
            .repos
            .iter()
            .filter_map(|r| Some((format!("repo.{}", r.name), r.path.as_ref()?))),
        "path",
        problems,
    );

    for repo in &draft.repos {
        let (Some(path), Some(worktree)) = (&repo.path, &repo.worktree) else {
            continue;
        };
        // Mutant (`mutant_config_worktree_root_inside_repo`): the inside check is skipped.
        if !cfg!(mutant_config_worktree_root_inside_repo)
            && Path::new(&worktree.root.key).starts_with(&path.key)
        {
            problems.push(Problem {
                location: worktree.root.at,
                message: format!(
                    "`repo.{}.worktree.root` is {}, inside the repository's path {}; \
                     worktrees are created outside the repository",
                    repo.name,
                    worktree.root.path.display(),
                    path.path.display()
                ),
            });
        }
    }

    if !cfg!(mutant_config_shared_dir) {
        for (what, field) in BUNDLE_PATHS {
            distinct(
                draft
                    .identities
                    .iter()
                    .filter_map(|i| Some((format!("identity.{}", i.name), field(i)?))),
                what,
                problems,
            );
        }
    }

    if let (Some(daily), reserve) = (draft.budget.daily_eqv, draft.budget.reserve_eqv)
        && reserve >= daily
    {
        problems.push(Problem {
            location: draft.reserve_at,
            message: format!(
                "`budget.reserve_eqv` is {reserve}, not less than `budget.daily_eqv` \
                 ({daily}); the reserve would leave nothing for the queue"
            ),
        });
    }

    for identity in &draft.identities {
        let Some(database) = &identity.database else {
            continue;
        };
        let Some(dir) = Path::new(&database.key).parent() else {
            problems.push(Problem {
                location: database.at,
                message: format!(
                    "`identity.{}.database` names a root, not a file",
                    identity.name
                ),
            });
            continue;
        };
        let disk = if cfg!(mutant_config_skip_disk_probe) {
            Disk::Local
        } else {
            probe.probe(dir)
        };
        let why = match disk {
            Disk::Local => continue,
            Disk::Network(why) | Disk::Synced(why) => why,
        };
        problems.push(Problem {
            location: database.at,
            message: format!(
                "the database of identity `{}` must be on a local disk, not synced or \
                 shared: {why}",
                identity.name
            ),
        });
    }
}

/// Refuses two owners of one path (by canonical key), naming both.
fn distinct<'d>(
    owners: impl Iterator<Item = (String, &'d PathValue)>,
    what: &str,
    problems: &mut Vec<Problem>,
) {
    let mut seen: BTreeMap<&str, String> = BTreeMap::new();
    for (owner, value) in owners {
        if let Some(first) = seen.get(value.key.as_str()) {
            problems.push(Problem {
                location: value.at,
                message: format!(
                    "`{first}` and `{owner}` share the {what} {}; each must have its own",
                    value.path.display()
                ),
            });
        } else {
            seen.insert(&value.key, owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use bosun_core::{PathStyle, canonical_path};

    use crate::{Disk, DiskProbe, Error, parse};

    /// A probe that reports the listed directories (by canonical key) and everything under
    /// them as the given disk, and the rest as local.
    struct FakeProbe(Vec<(String, Disk)>);

    impl FakeProbe {
        fn local() -> Self {
            Self(Vec::new())
        }
    }

    impl DiskProbe for FakeProbe {
        fn probe(&self, dir: &Path) -> Disk {
            let key = canonical_path(&dir.to_string_lossy(), PathStyle::host());
            self.0
                .iter()
                .find(|(root, _)| key.starts_with(root.as_str()))
                .map_or(Disk::Local, |(_, disk)| disk.clone())
        }
    }

    /// An absolute path on this host, written with `/` after the root.
    fn abs(rest: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{}", rest.replace('/', "\\"))
        } else {
            format!("/{rest}")
        }
    }

    fn problems(text: &str, probe: &dyn DiskProbe) -> Vec<String> {
        match parse(text, probe) {
            Ok(_) => Vec::new(),
            Err(Error::Invalid(problems)) => problems.iter().map(ToString::to_string).collect(),
            Err(e) => panic!("{e}"),
        }
    }

    fn identity(name: &str, config_dir: &str, database: &str) -> String {
        format!(
            "[identity.{name}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
            abs(config_dir),
            abs(database)
        )
    }

    #[test]
    fn identities_sharing_a_directory_or_a_database_are_refused() {
        let probe = FakeProbe::local();
        let ok = identity("personal", "home/.claude", "data/personal.db")
            + &identity("work", "home/.claude-work", "data/work.db");
        assert_eq!(problems(&ok, &probe), Vec::<String>::new());

        // The same directory, written differently: only the canonical keys are equal on
        // Windows, where case and separators do not matter.
        let other_spelling = if cfg!(windows) {
            "HOME/.Claude/"
        } else {
            "home//.claude/"
        };
        let shared_dir = identity("personal", "home/.claude", "data/personal.db")
            + &identity("work", other_spelling, "data/work.db");
        let found = problems(&shared_dir, &probe);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains(
                "`identity.personal` and `identity.work` share the Claude configuration directory"
            ),
            "{found:?}"
        );
        assert!(found[0].starts_with("line 5, column 21:"), "{found:?}");

        let shared_db = identity("personal", "home/.claude", "data/one.db")
            + &identity("work", "home/.claude-work", "data/one.db");
        let found = problems(&shared_db, &probe);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`identity.personal` and `identity.work` share the database file"),
            "{found:?}"
        );
    }

    #[test]
    fn a_database_on_a_network_or_synced_disk_is_refused() {
        let config = identity("personal", "home/.claude", "local/personal.db")
            + &identity("work", "home/.claude-work", "share/work.db")
            + &identity("third", "home/.claude-third", "sync/third.db");
        let key = |rest: &str| canonical_path(&abs(rest), PathStyle::host());
        let probe = FakeProbe(vec![
            (key("share"), Disk::Network("a test share".to_owned())),
            (key("sync"), Disk::Synced("a test sync client".to_owned())),
        ]);
        let found = problems(&config, &probe);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].contains("identity `work` must be on a local disk")
                && found[0].contains("a test share"),
            "{found:?}"
        );
        assert!(
            found[1].contains("identity `third` must be on a local disk")
                && found[1].contains("a test sync client"),
            "{found:?}"
        );
    }

    #[test]
    fn a_repository_binds_a_configured_identity() {
        let config = identity("personal", "home/.claude", "data/personal.db")
            + &format!(
                "[repo.a]\npath = '{}'\nidentity = 'personal'\n\
                 [repo.b]\npath = '{}'\nidentity = 'work'\n",
                abs("git/a"),
                abs("git/b")
            );
        let found = problems(&config, &FakeProbe::local());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`repo.b` binds identity `work`, which is not configured"),
            "{found:?}"
        );
    }

    #[test]
    fn two_repositories_at_one_path_are_refused() {
        let config = identity("personal", "home/.claude", "data/personal.db")
            + &format!(
                "[repo.a]\npath = '{}'\nidentity = 'personal'\n\
                 [repo.b]\npath = '{}'\nidentity = 'personal'\n",
                abs("git/a"),
                abs("git/./a/")
            );
        let found = problems(&config, &FakeProbe::local());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`repo.a` and `repo.b` share the path"),
            "{found:?}"
        );
    }

    #[test]
    fn repository_keys_are_canonical() {
        let config = identity("personal", "home/.claude", "data/personal.db")
            + &format!(
                "[repo.a]\npath = '{}'\nidentity = 'personal'\n",
                abs("git/Bosun/")
            );
        let config = parse(&config, &FakeProbe::local()).unwrap();
        let repo = &config.repos["a"];
        assert_eq!(repo.path, PathBuf::from(abs("git/Bosun/")));
        assert_eq!(
            repo.key,
            canonical_path(&abs("git/Bosun"), PathStyle::host())
        );
        assert_eq!(repo.identity, "personal");
    }
}
