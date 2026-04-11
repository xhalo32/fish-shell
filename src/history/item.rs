use fish_widestring::prelude::*;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub enum HistoryItem {
    LocalHistoryItem(LocalHistoryItem),
    SharedHistoryItem(fish_history_api::HistoryItem),
}

impl HistoryItem {
    pub fn str(&self) -> &wstr {
        match self {
            HistoryItem::LocalHistoryItem(item) => item.str(),
            HistoryItem::SharedHistoryItem(item) => item.str(),
        }
    }

    pub fn into_str(self) -> WString {
        match self {
            HistoryItem::LocalHistoryItem(item) => item.into_str(),
            HistoryItem::SharedHistoryItem(item) => item.into_str(),
        }
    }

    pub fn get_timestamp(&self) -> SystemTime {
        match self {
            HistoryItem::LocalHistoryItem(item) => item.creation_timestamp,
            HistoryItem::SharedHistoryItem(item) => item.get_timestamp(),
        }
    }
}

impl From<LocalHistoryItem> for HistoryItem {
    fn from(item: LocalHistoryItem) -> Self {
        HistoryItem::LocalHistoryItem(item)
    }
}

impl From<fish_history_api::HistoryItem> for HistoryItem {
    fn from(item: fish_history_api::HistoryItem) -> Self {
        HistoryItem::SharedHistoryItem(item)
    }
}

/// Ways that a history item may be written to disk (or omitted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceMode {
    /// The history item is stored in-memory only, not written to disk
    Memory,
    /// The history item is stored in-memory and deleted when a new item is added
    Ephemeral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalHistoryItem {
    /// The actual contents of the entry.
    contents: WString,
    /// Original creation time for the entry.
    creation_timestamp: SystemTime,
    pub persist_mode: PersistenceMode,
}

impl LocalHistoryItem {
    /// Construct from a text, timestamp, and a persistence mode (in-memory/ephemeral).
    pub fn new(s: WString, when: SystemTime, persist_mode: PersistenceMode) -> Self {
        Self {
            contents: s,
            creation_timestamp: when,
            persist_mode,
        }
    }

    /// Returns the text as a string.
    pub fn str(&self) -> &wstr {
        &self.contents
    }

    pub fn into_str(self) -> WString {
        self.contents
    }

    /// Returns whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }

    /// Returns the timestamp for creating this history item.
    pub fn timestamp(&self) -> SystemTime {
        self.creation_timestamp
    }

    pub fn is_ephemeral(&self) -> bool {
        self.persist_mode == PersistenceMode::Ephemeral
    }

    pub fn merge(&mut self, item: &LocalHistoryItem) -> bool {
        // The logic when to merge here is not particularly important. Ephemeral commands get removed from the history anyways
        if self.str() != item.str() || self.persist_mode != item.persist_mode {
            false
        } else {
            self.creation_timestamp = self.creation_timestamp.max(item.creation_timestamp);
            true
        }
    }
}
