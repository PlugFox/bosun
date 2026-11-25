//! Cross-checks of a draft: identity isolation (#2), the local-disk rule for databases, the
//! budget's reserve, worktree roots outside their repository, the landing modes this version
//! carries out and shared build directories (#8).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosun_core::{PathStyle, strip_root};

use crate::disk::{Disk, DiskProbe};
use crate::error::Problem;
use crate::read::{Draft, DraftIdentity, DraftRepo, DraftSharedBuild, PathValue};
use crate::{LandingMode, Resource};

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
        let root = repo.worktree.as_ref().and_then(|w| w.root.as_ref());
        if let (Some(path), Some(root)) = (&repo.path, root) {
            worktree_root_outside(&repo.name, path, root, problems);
        }
        landing(repo, problems);
        if let Some(shared) = repo.worktree.as_ref().and_then(|w| w.shared_build.as_ref()) {
            shared_build(
                &repo.name,
                shared,
                repo.path.as_ref(),
                root,
                &draft.resources,
                problems,
            );
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

/// Refuses a worktree root that is the repository's path or under it (#8): worktrees made
/// there would be files of the main checkout. Canonical keys are compared, so on Windows case
/// and separators do not matter.
fn worktree_root_outside(
    repo: &str,
    path: &PathValue,
    root: &PathValue,
    problems: &mut Vec<Problem>,
) {
    // Mutant (`mutant_config_worktree_root_inside_repo`): the inside check is skipped.
    let inside = !cfg!(mutant_config_worktree_root_inside_repo)
        && strip_root(&root.key, &path.key, PathStyle::host()).is_some();
    if inside {
        problems.push(Problem {
            location: root.at,
            message: format!(
                "`repo.{repo}.worktree.root` {} is inside the repository's path {}; worktrees \
                 go outside the checkout",
                root.path.display(),
                path.path.display()
            ),
        });
    }
}

/// Refuses a landing this version cannot carry out: `merge` and `pr` without a worktree (E19:
/// every new path is gated on `repo.<name>.worktree`), and `merge.push = true`.
fn landing(repo: &DraftRepo, problems: &mut Vec<Problem>) {
    let name = &repo.name;
    let draft = &repo.landing;
    let mode = draft.landing.mode;
    let word = match mode {
        LandingMode::Merge => "merge",
        LandingMode::Pr => "pr",
        LandingMode::Local => "local",
    };
    // Mutant (`mutant_config_landing_without_worktree`): `merge` is accepted without a
    // worktree, and would land from the main checkout.
    let unchecked = cfg!(mutant_config_landing_without_worktree) && mode == LandingMode::Merge;
    if !unchecked && mode != LandingMode::Local && repo.worktree.is_none() {
        problems.push(Problem {
            location: draft.mode_at,
            message: format!(
                "`repo.{name}.landing.mode` is \"{word}\", which needs `repo.{name}.worktree`: \
                 the item lands from its own worktree, e.g. `worktree = {{ root = '...' }}`"
            ),
        });
    }
    if draft.landing.merge.push {
        problems.push(Problem {
            location: draft.push_at,
            message: format!(
                "`repo.{name}.landing.merge.push` is true, which is not available yet; the \
                 target is not pushed"
            ),
        });
    }
}

/// Refuses a shared build directory (E1) that is not guarded by an existing exclusive resource,
/// or that lies in the repository's checkout or under its worktree root, or holds either: cargo
/// writes its own files next to them, and a `cargo clean` deletes the whole directory.
fn shared_build(
    repo: &str,
    shared: &DraftSharedBuild,
    path: Option<&PathValue>,
    root: Option<&PathValue>,
    resources: &BTreeMap<String, Resource>,
    problems: &mut Vec<Problem>,
) {
    let what = format!("repo.{repo}.worktree.shared_build");
    // Mutant (`mutant_config_shared_build_any_resource`): the resource is not checked.
    if let Some((name, at)) = &shared.resource
        && !cfg!(mutant_config_shared_build_any_resource)
    {
        let why = match resources.get(name) {
            None => Some("which is not configured".to_owned()),
            Some(resource) if resource.capacity != 1 => Some(format!(
                "whose capacity is {}; the build directory takes one holder at a time, so \
                 `capacity = 1`",
                resource.capacity
            )),
            Some(_) => None,
        };
        if let Some(why) = why {
            problems.push(Problem {
                location: *at,
                message: format!("`{what}.resource` names `resource.{name}`, {why}"),
            });
        }
    }
    let Some(dir) = &shared.dir else {
        return;
    };
    for (owner, value) in [("the repository's path", path), ("`worktree.root`", root)] {
        let Some(value) = value else {
            continue;
        };
        let style = PathStyle::host();
        let relation = if strip_root(&dir.key, &value.key, style).is_some() {
            "is inside"
        } else if !cfg!(mutant_config_shared_build_holder_allowed)
            // Mutant (`mutant_config_shared_build_holder_allowed`): a `dir` that holds it passes.
            && strip_root(&value.key, &dir.key, style).is_some()
        {
            "holds"
        } else {
            continue;
        };
        problems.push(Problem {
            location: dir.at,
            message: format!(
                "`{what}.dir` {} {relation} {owner} {}; the shared build directory lies outside \
                 both and holds neither",
                dir.path.display(),
                value.path.display()
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
