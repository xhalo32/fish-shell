use bitflags::bitflags;
use fish_history_api::HistoryProvider;
use fish_wcstringutil::subsequence_in_string;
use fish_widestring::{prelude::*, subslice_position};
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::parse_util::unescape_wildcards;
use crate::wildcard::{ANY_STRING, wildcard_match};

use crate::common::CancelChecker;
use crate::history::{History, HistoryItem};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchType {
    /// Search for commands exactly matching the given string.
    Exact,
    /// Search for commands containing the given string.
    Contains,
    /// Search for commands starting with the given string.
    Prefix,
    /// Search for commands where any line matches the given string.
    LinePrefix,
    /// Search for commands containing the given glob pattern.
    ContainsGlob,
    /// Search for commands starting with the given glob pattern.
    PrefixGlob,
    /// Search for commands containing the given string as a subsequence
    ContainsSubsequence,
}

/// Perform a search of `hist` for `search_string`. Invoke a function `func` for each match. If
/// `func` returns [`ControlFlow::Break`], stop the search.
pub fn do_1_history_search<P: HistoryProvider>(
    hist: Arc<History<P>>,
    search_type: SearchType,
    search_string: WString,
    case_sensitive: bool,
    mut func: impl FnMut(&HistoryItem) -> ControlFlow<(), ()>,
    cancel_check: &CancelChecker,
) {
    let mut searcher = HistorySearch::new_with(
        hist,
        search_string,
        search_type,
        if case_sensitive {
            SearchFlags::empty()
        } else {
            SearchFlags::IGNORE_CASE
        },
        0,
    );
    while !cancel_check() && searcher.go_to_next_match(SearchDirection::Backward) {
        if let ControlFlow::Break(()) = func(searcher.current_item()) {
            break;
        }
    }
}

bitflags! {
    /// Flags for history searching.
    #[derive(Clone, Copy, Default)]
    pub struct SearchFlags: u32 {
        /// If set, ignore case.
        const IGNORE_CASE = 1 << 0;
        /// If set, do not deduplicate, which can help performance.
        const NO_DEDUP = 1 << 1;
    }
}

impl HistoryItem {
    /// Returns whether our contents matches a search term.
    pub fn matches_search(&self, term: &wstr, typ: SearchType, case_sensitive: bool) -> bool {
        // Note that 'term' has already been lowercased when constructing the
        // search object if we're doing a case insensitive search.
        let content_to_match = if case_sensitive {
            Cow::Borrowed(self.str())
        } else {
            Cow::Owned(self.str().to_lowercase())
        };

        match typ {
            SearchType::Exact => term == *content_to_match,
            SearchType::Contains => {
                subslice_position(content_to_match.as_slice(), term.as_slice()).is_some()
            }
            SearchType::Prefix => content_to_match.as_slice().starts_with(term.as_slice()),
            SearchType::LinePrefix => content_to_match
                .as_char_slice()
                .split(|&c| c == '\n')
                .any(|line| line.starts_with(term.as_char_slice())),
            SearchType::ContainsGlob => {
                let mut pat = unescape_wildcards(term);
                if !pat.starts_with(ANY_STRING) {
                    pat.insert(0, ANY_STRING);
                }
                if !pat.ends_with(ANY_STRING) {
                    pat.push(ANY_STRING);
                }
                wildcard_match(content_to_match.as_ref(), &pat, false)
            }
            SearchType::PrefixGlob => {
                let mut pat = unescape_wildcards(term);
                if !pat.ends_with(ANY_STRING) {
                    pat.push(ANY_STRING);
                }
                wildcard_match(content_to_match.as_ref(), &pat, false)
            }
            SearchType::ContainsSubsequence => subsequence_in_string(term, &content_to_match),
        }
    }
}

/// Support for searching a history backwards.
/// Note this does NOT de-duplicate; it is the caller's responsibility to do so.
pub struct HistorySearch<P: HistoryProvider> {
    /// The history in which we are searching.
    history: Arc<History<P>>,
    /// The original search term.
    orig_term: WString,
    /// The (possibly lowercased) search term.
    canon_term: WString,
    /// Our search type.
    search_type: SearchType, // history_search_type_t::contains
    /// Our flags.
    flags: SearchFlags, // 0
    /// The current history item.
    current_item: Option<HistoryItem>,
    /// Index of the current history item.
    current_index: usize, // 0
    /// If deduping, the items we've seen.
    deduper: HashSet<WString>,
}

impl<P: HistoryProvider> HistorySearch<P> {
    #[cfg(test)]
    fn new(hist: Arc<History<P>>, s: WString) -> Self {
        Self::new_with(hist, s, SearchType::Contains, SearchFlags::default(), 0)
    }
    #[cfg(test)]
    fn new_with_type(hist: Arc<History<P>>, s: WString, search_type: SearchType) -> Self {
        Self::new_with(hist, s, search_type, SearchFlags::default(), 0)
    }
    #[cfg(test)]
    fn new_with_flags(hist: Arc<History<P>>, s: WString, flags: SearchFlags) -> Self {
        Self::new_with(hist, s, SearchType::Contains, flags, 0)
    }
    /// Constructs a new history search.
    pub fn new_with(
        hist: Arc<History<P>>,
        s: WString,
        search_type: SearchType,
        flags: SearchFlags,
        starting_index: usize,
    ) -> Self {
        let mut search = Self {
            history: hist,
            orig_term: s.clone(),
            canon_term: s,
            search_type,
            flags,
            current_item: None,
            current_index: starting_index,
            deduper: HashSet::new(),
        };

        if search.ignores_case() {
            search.canon_term = search.canon_term.to_lowercase();
        }

        search
    }

    /// Returns the original search term.
    pub fn original_term(&self) -> &wstr {
        &self.orig_term
    }

    pub fn prepare_to_search_after_deletion(&mut self) {
        assert_ne!(self.current_index, 0);
        self.current_index -= 1;
        self.current_item = None;
    }

    /// Finds the next search result. Returns `true` if one was found.
    pub fn go_to_next_match(&mut self, direction: SearchDirection) -> bool {
        let invalid_index = match direction {
            SearchDirection::Backward => usize::MAX,
            SearchDirection::Forward => 0,
        };

        if self.current_index == invalid_index {
            return false;
        }

        let mut index = self.current_index;
        loop {
            // Backwards means increasing our index.
            match direction {
                SearchDirection::Backward => index += 1,
                SearchDirection::Forward => index -= 1,
            }

            if self.current_index == invalid_index {
                return false;
            }

            // We're done if it's empty or we cancelled.
            let Some(item) = self.history.item_at_index(index) else {
                self.current_index = match direction {
                    SearchDirection::Backward => self.history.size() + 1,
                    SearchDirection::Forward => 0,
                };
                self.current_item = None;
                return false;
            };

            // Look for an item that matches and (if deduping) that we haven't seen before.
            if !item.matches_search(&self.canon_term, self.search_type, !self.ignores_case()) {
                continue;
            }

            // Skip if deduplicating.
            if self.dedup() && !self.deduper.insert(item.str().to_owned()) {
                continue;
            }

            // This is our new item.
            self.current_item = Some(item);
            self.current_index = index;
            return true;
        }
    }

    /// Move current index so there is `value` matches in between new and old indexes
    pub fn search_forward(&mut self, value: usize) {
        while self.go_to_next_match(SearchDirection::Forward) && self.deduper.len() <= value {}
        self.deduper.clear();
    }

    /// Returns the current search result item.
    ///
    /// # Panics
    ///
    /// This function panics if there is no current item.
    pub fn current_item(&self) -> &HistoryItem {
        self.current_item.as_ref().expect("No current item")
    }

    pub fn canon_term(&self) -> &wstr {
        &self.canon_term
    }

    /// Returns the current search result item contents.
    ///
    /// # Panics
    ///
    /// This function panics if there is no current item.
    pub fn current_string(&self) -> &wstr {
        self.current_item().str()
    }

    /// Returns the index of the current history item.
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    /// Returns whether we are case insensitive.
    pub fn ignores_case(&self) -> bool {
        self.flags.contains(SearchFlags::IGNORE_CASE)
    }

    /// Returns whether we deduplicate items.
    fn dedup(&self) -> bool {
        !self.flags.contains(SearchFlags::NO_DEDUP)
    }
}
