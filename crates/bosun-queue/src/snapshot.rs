//! The snapshot of #5: at claim the daemon stores the body, the parsed block and the comments it
//! will inline, and their hash. The verdict always runs from the snapshot, never from the live
//! issue.
//!
//! The hash covers what the agent and the verdict are given, normalised so that formatting the
//! backend adds does not change it: line endings are LF, and whitespace before and after the
//! body (and each comment) is dropped. Everything inside the body counts.

use sha2::{Digest, Sha256};

use crate::card::{Card, Comment};
use crate::contract::{Contract, ContractError};

/// What an item is claimed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The normalised body.
    pub body: String,
    /// The contract parsed from it.
    pub contract: Contract,
    /// The comments to inline, as chosen by the caller (the author allowlist of #2 is the
    /// caller's), with normalised bodies.
    pub comments: Vec<Comment>,
    /// SHA-256 of the above, lowercase hex.
    pub hash: String,
}

/// `text` with LF line endings and no leading or trailing whitespace.
#[must_use]
pub fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}

impl Snapshot {
    /// Snapshots `card` with the comments `inline` keeps.
    ///
    /// # Errors
    ///
    /// The body's contract does not parse.
    pub fn take(
        card: &Card,
        inline: impl Fn(&Comment) -> bool,
    ) -> Result<Self, Vec<ContractError>> {
        let body = normalise(&card.body);
        let contract = Contract::parse(&body)?;
        let comments: Vec<Comment> = card
            .comments
            .iter()
            .filter(|c| inline(c))
            .map(|c| Comment {
                author: c.author.clone(),
                key: c.key.clone(),
                body: normalise(&c.body),
            })
            .collect();
        let hash = hash(&body, &comments);
        Ok(Self {
            body,
            contract,
            comments,
            hash,
        })
    }
}

fn hash(body: &str, comments: &[Comment]) -> String {
    let mut hasher = Sha256::new();
    let mut field = |text: &str| {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    };
    field("bosun-snapshot-1");
    field(body);
    for comment in comments {
        field(&comment.author);
        field(&comment.body);
    }
    crate::hex(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Column, ItemId};

    fn card(body: &str) -> Card {
        Card {
            id: ItemId(1),
            title: "t".into(),
            column: Column::Ready,
            body: body.into(),
            labels: Vec::new(),
            draft: false,
            comments: vec![Comment {
                author: "me".into(),
                key: None,
                body: "Also check the docs.".into(),
            }],
            blocked_by: Vec::new(),
        }
    }

    const BODY: &str = "Do it.\n\n```bosun\nverdict = \"cargo test\"\n```\n";

    fn take(body: &str) -> Snapshot {
        Snapshot::take(&card(body), |_| true).unwrap()
    }

    #[test]
    fn changing_the_body_changes_the_hash() {
        let base = take(BODY);
        assert_eq!(base.hash.len(), 64);
        assert_ne!(base.hash, take(&BODY.replace("Do it.", "Do it now.")).hash);
        assert_ne!(
            base.hash,
            take(&BODY.replace("cargo test", "cargo test -q")).hash
        );
        assert_ne!(base.hash, take(&BODY.replace("\n\n```", "\n\n\n```")).hash);
    }

    #[test]
    fn whitespace_outside_the_body_does_not() {
        let base = take(BODY);
        assert_eq!(base.hash, take(&format!("\n\n  {BODY}\n\n\t")).hash);
        assert_eq!(base.hash, take(&BODY.replace('\n', "\r\n")).hash);
        let mut spaced = card(BODY);
        spaced.comments[0].body = "\r\nAlso check the docs.\r\n\r\n".into();
        assert_eq!(base.hash, Snapshot::take(&spaced, |_| true).unwrap().hash);
    }

    #[test]
    fn the_inlined_comments_count() {
        let all = take(BODY);
        let none = Snapshot::take(&card(BODY), |_| false).unwrap();
        assert_eq!(none.comments, []);
        assert_ne!(all.hash, none.hash);
        assert_eq!(all.contract.verdict, "cargo test");
    }
}
