use std::time::SystemTime;

use crate::prelude::*;
use fish_history_api::HistoryItem;
use lru::LruCache;

pub trait LruCacheExt {
    /// Function to add a history item.
    fn add_item(&mut self, item: HistoryItem);
}

impl LruCacheExt for LruCache<WString, HistoryItem> {
    fn add_item(&mut self, item: HistoryItem) {
        // Skip empty items.
        if item.is_empty() {
            return;
        }

        // See if it's in the cache. If it is, update the timestamp. If not, we create a new node
        // and add it. Note that calling get_node promotes the node to the front.
        let key = item.str();
        if let Some(node) = self.get_mut(key) {
            node.set_timestamp(SystemTime::max(node.get_timestamp(), item.get_timestamp()));
            // What to do about paths here? Let's just ignore them.
        } else {
            self.put(key.to_owned(), item);
        }
    }
}
