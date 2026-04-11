use std::time::SystemTime;

pub use fish_widestring;
use fish_widestring::prelude::*;

/// A command line history entry plus metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryItem {
    /// The actual contents of the entry.
    contents: WString,
    /// Original creation time for the entry.
    creation_timestamp: SystemTime,
}

impl HistoryItem {
    pub fn new(s: WString, when: SystemTime) -> Self {
        Self {
            contents: s,
            creation_timestamp: when,
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

    // TODO should not be here
    // pub fn matches_search(&self, term: &wstr, typ: SearchType, case_sensitive: bool) -> bool {

    /// Returns the timestamp for creating this history item.
    pub fn get_timestamp(&self) -> SystemTime {
        self.creation_timestamp
    }

    pub fn set_timestamp(&mut self, timestamp: SystemTime) {
        self.creation_timestamp = timestamp;
    }

    /// We can merge two items if they are the same command simply by taking the maximum of their timestamps.
    /// Returns false if the items were not merged.
    pub fn merge(&mut self, item: &HistoryItem) -> bool {
        if self.str() != item.str() {
            false
        } else {
            self.creation_timestamp = self.creation_timestamp.max(item.creation_timestamp);
            true
        }
    }
}

/// A history provider.
///
/// All methods must be callable on shared references and the implementor should provide required locking mechanisms.
pub trait HistoryProvider: Send + Sync {
    fn name(&self) -> &wstr;
    fn item_at_index(&self, idx: usize) -> Option<HistoryItem>;
    fn get_history(&self) -> Vec<HistoryItem> {
        let mut result = Vec::new();
        let mut idx = 1;
        while let Some(item) = self.item_at_index(idx) {
            result.push(item.to_owned());
            idx += 1;
        }
        result
    }
    fn add(&self, item: HistoryItem);
    fn remove(&self, s: &wstr);
    fn clear(&self);
    fn size(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.size() == 0
    }
    fn save(&self);
}
