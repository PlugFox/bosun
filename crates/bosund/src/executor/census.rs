//! The census of a repository's tests after a passing verdict (#7).
//!
//! When `repo.<name>.policy.census` is set, a verdict run of the item's own commits that
//! passed (in `Verifying`, never a landing's re-verdict) is followed by a count of the census's
//! counters in two commits' trees: the base of the item's own commits and the commit the
//! verdict judged ([`bosun_git::census`]). A `keep` counter that dropped or a `cap` counter
//! that rose, and that the card's `allow-census` does not name, replaces the verdict's event
//! with [`Event::CensusDropped`]: Needs you (`census_drop`), no attempt counted. The figures
//! are stored as the item's [`NOTE_CENSUS`] note, which its Needs-you comment quotes. A census
//! that cannot count is the same Needs you, its note holding the error (fail closed).

use bosun_config::Census;
use bosun_core::publish;
use bosun_core::{Event, ItemId};
use bosun_git::census::{Finding, compare, count};
use bosun_store::Write;

use super::{Claimed, Shared, blocking, git, now_ms};

/// The kind of an item's note that holds what its census found: a JSON array of
/// `{counter, base, head}`, or a JSON object `{error}` when the census could not count.
pub(crate) const NOTE_CENSUS: &str = "census";

/// The census of attempt `attempt`, run `run`, whose verdict passed: `Some` event that replaces
/// the verdict's when a counter moved the wrong way or the census failed.
pub(crate) async fn after_pass(
    shared: &Shared,
    claimed: &Claimed,
    census: &Census,
    item: &ItemId,
    attempt: u32,
    run: u32,
) -> Option<Event> {
    let allowed = if cfg!(mutant_loop_census_ignores_allowance) {
        // Mutant (`mutant_loop_census_ignores_allowance`): the card's `allow-census` is not
        // passed, so an allowed move is a finding all the same.
        Vec::new()
    } else {
        claimed.snapshot.contract.allow_census.clone()
    };
    let counted = {
        let census = census.clone();
        let workdir = claimed.workdir.clone();
        let base = claimed.base.clone();
        let base_ref = claimed.worktree.as_ref().map(|wt| wt.base_ref.clone());
        blocking(move || find(&census, &workdir, &base, base_ref.as_deref(), &allowed)).await
    };
    let (body, trouble) = match counted {
        Ok(findings) if findings.is_empty() => return None,
        Ok(findings) => {
            let figures: Vec<String> = findings.iter().map(figure).collect();
            let body = serde_json::Value::Array(
                findings
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "counter": f.counter,
                            "base": f.base,
                            "head": f.head,
                        })
                    })
                    .collect(),
            );
            (
                body.to_string(),
                format!(
                    "{item}: attempt {attempt}'s verdict passed, but the census of the tests \
                     moved the wrong way: {}; the item goes to Needs you",
                    figures.join(", ")
                ),
            )
        }
        Err(e) => (
            serde_json::json!({ "error": e.to_string() }).to_string(),
            format!(
                "{item}: attempt {attempt}'s verdict passed, but the census of the tests cannot \
                 count: {e}; the item goes to Needs you"
            ),
        ),
    };
    shared.trouble(trouble);
    let note = Write::ItemNote {
        item: item.clone(),
        kind: NOTE_CENSUS.to_owned(),
        body,
        at_ms: now_ms(),
    };
    if let Err(e) = shared.deps.writer.write(vec![note]).await {
        shared.trouble(format!("the census note of {item} cannot be stored: {e}"));
    }
    Some(Event::CensusDropped { attempt, run })
}

/// The counters of `census` that moved the wrong way from the base of the item's own commits
/// to the commit the verdict judged (the HEAD of `workdir`), less `allowed`. In worktree mode
/// (`base_ref`) the base is where the item's commits start ([`git::item_base`]).
fn find(
    census: &Census,
    workdir: &std::path::Path,
    base: &bosun_git::Base,
    base_ref: Option<&str>,
    allowed: &[String],
) -> Result<Vec<Finding>, bosun_git::Error> {
    let git = bosun_git::Git::new(workdir);
    let base = match base_ref {
        Some(target) => git::item_base(&git, base, target)?.head,
        None => base.head.clone(),
    };
    let head = git::head(&git)?;
    let counters: Vec<(&str, &str)> = census
        .keep
        .iter()
        .chain(&census.cap)
        .map(|(name, regex)| (name.as_str(), regex.as_str()))
        .collect();
    let at_base = count(&git, &base, &census.glob, &counters)?;
    let at_head = count(&git, &head, &census.glob, &counters)?;
    let keep: Vec<&str> = census.keep.keys().map(String::as_str).collect();
    let cap: Vec<&str> = census.cap.keys().map(String::as_str).collect();
    Ok(compare(&at_base, &at_head, &keep, &cap, allowed))
}

/// `assertions 2 -> 0`.
pub(crate) fn figure(finding: &Finding) -> String {
    format!("{} {} -> {}", finding.counter, finding.base, finding.head)
}

/// The names of the card's `allow-census` that `census` does not have, logged once per item
/// at its first census: they allow nothing.
pub(crate) fn log_unknown_allowances(
    repo: &str,
    item: &ItemId,
    census: &Census,
    allowed: &[String],
) {
    for name in allowed {
        if !census.keep.contains_key(name) && !census.cap.contains_key(name) {
            crate::log::log!(
                "bosund: repository {repo}: {item} allows the census counter {name:?}, which \
                 repo.{repo}.policy.census does not have"
            );
        }
    }
}

/// What the Needs-you comment adds after its reason from the item's [`NOTE_CENSUS`] note: a
/// list of the findings (`assertions 2 -> 0`) and how the card accepts them, or the error that
/// stopped the census; empty when the note says neither.
pub(crate) fn comment_figures(note: &str) -> String {
    let lines = comment_lines(note);
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from(":");
    for line in &lines {
        out.push_str("\n- ");
        out.push_str(line);
    }
    out.push_str(
        "\n\n`allow-census = [\"<counter>\"]` in the card's bosun block accepts a counter's move",
    );
    out
}

/// The lines of the item's [`NOTE_CENSUS`] note: one per finding, or the error that stopped
/// the census.
fn comment_lines(note: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Stored {
        counter: String,
        base: u64,
        head: u64,
    }
    #[derive(serde::Deserialize)]
    struct Failed {
        error: String,
    }
    if let Ok(findings) = serde_json::from_str::<Vec<Stored>>(note) {
        return findings
            .into_iter()
            .map(|f| {
                figure(&Finding {
                    counter: f.counter,
                    base: f.base,
                    head: f.head,
                })
            })
            .collect();
    }
    serde_json::from_str::<Failed>(note).map_or_else(
        |_| Vec::new(),
        |f| {
            let error = publish::sanitise_inline(&publish::scrub(&f.error), 500);
            vec![format!("the census could not count: {error}")]
        },
    )
}
