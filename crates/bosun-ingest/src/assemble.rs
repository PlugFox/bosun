//! Records to assembled messages, in memory: the fold the store performs row by row (#3,
//! "Dedup and assembly"). Every rule is order-independent, so the records of several files may
//! be added in any order.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::{Message, merge};

use crate::record::{FileTouch, Record, SessionCheck, ToolResult};

/// A fold of records from any number of files.
#[derive(Debug, Default, Clone)]
pub struct Assembly {
    /// By (provider, message id).
    messages: BTreeMap<(String, String), Message>,
    /// By (provider, `tool_use` id).
    results: BTreeMap<(String, String), ToolResult>,
    touches: BTreeSet<FileTouch>,
    /// By (provider, session id): the record with the largest offset.
    checks: BTreeMap<(String, String), SessionCheck>,
}

impl Assembly {
    /// Folds one record in.
    pub fn add(&mut self, record: Record) {
        match record {
            Record::Message(message) => {
                let key = (message.provider.clone(), message.id.clone());
                let merged = match self.messages.remove(&key) {
                    // The key is the merge key, so the two always match.
                    Some(known) => merge(known, message.clone()).unwrap_or(message),
                    None => message,
                };
                self.messages.insert(key, merged);
            }
            Record::ToolResult(result) => {
                let key = (result.provider.clone(), result.tool_use_id.clone());
                self.results
                    .entry(key)
                    .and_modify(|known| {
                        known.result_bytes = known.result_bytes.max(result.result_bytes);
                        known.is_error |= result.is_error;
                    })
                    .or_insert(result);
            }
            Record::FileTouch(touch) => {
                self.touches.insert(touch);
            }
            Record::SessionCheck(check) => {
                let key = (check.provider.clone(), check.session_id.clone());
                match self.checks.get(&key) {
                    Some(known) if known.offset >= check.offset => {}
                    _ => {
                        self.checks.insert(key, check);
                    }
                }
            }
        }
    }

    /// Every message, with the results of its tool calls applied, ordered by (provider, id).
    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        self.messages
            .values()
            .map(|message| {
                let mut message = message.clone();
                for (id, call) in &mut message.tool_calls {
                    if let Some(result) = self.results.get(&(message.provider.clone(), id.clone()))
                    {
                        call.result_bytes = call.result_bytes.max(result.result_bytes);
                        call.is_error |= result.is_error;
                    }
                }
                message
            })
            .collect()
    }

    /// Every file touch, deduplicated.
    pub fn touches(&self) -> impl Iterator<Item = &FileTouch> {
        self.touches.iter()
    }

    /// The last session check of each (provider, session).
    pub fn checks(&self) -> impl Iterator<Item = &SessionCheck> {
        self.checks.values()
    }
}
