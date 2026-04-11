//! Fish supports multiple shells writing to history at once. Here is its strategy:
//!
//! 1. All history files are append-only. Data, once written, is never modified.
//!
//! 2. A history file may be re-written ("vacuumed"). This involves reading in the file and writing
//!    a new one, while performing maintenance tasks: discarding items in an LRU fashion until we
//!    reach the desired maximum count, removing duplicates, and sorting them by timestamp
//!    (eventually, not implemented yet). The new file is atomically moved into place via `rename()`.
//!
//! 3. History files are mapped in via `mmap()`. This allows only storing one `usize` per item (its
//!    offset), and lazily loading items on demand, which reduces memory consumption.
//!
//! 4. Accesses to the history file need to be synchronized. This is achieved by functionality in
//!    `src/fs.rs`. By default, `flock()` is used for locking. If that is unavailable, an imperfect
//!    fallback solution attempts to detect races and retries if a race is detected.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufWriter, Read, Write},
    num::NonZeroUsize,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    fds::wopen_cloexec,
    flog, flogf,
    fs::{
        LOCKED_FILE_MODE, LockedFile, LockingMode, PotentialUpdate, WriteMethod, fsync,
        lock_and_load, rewrite_via_temporary_file,
    },
    history::yaml::{
        cache::LruCacheExt,
        deletion_scope::DeletionScope,
        file::{HistoryFile, RawHistoryFile, write_to},
        time_profiler::TimeProfiler,
    },
    path::{path_get_config, path_get_data},
    prelude::*,
    wutil::{FileId, INVALID_FILE_ID, file_id_for_file, wrealpath, wstat, wunlink},
};
use fish_history_api::{HistoryItem, HistoryProvider};
use lru::LruCache;
use nix::{fcntl::OFlag, sys::stat::Mode};
use rand::Rng;

pub const VACUUM_FREQUENCY: usize = 25;

struct HistoryImpl {
    /// The name of the YAMLHistory parent.
    cached_name: WString,
    /// Optional custom directory for the history file. If None, uses path_get_data().
    /// Primarily for testing.
    custom_directory: Option<WString>,
    /// New items to save to the history file. We need to keep these around so we can
    /// distinguish between items in our history and items in the history of other shells that were
    /// started after we were started.
    new_items: Vec<HistoryItem>,
    /// The index of the first new item that we have not yet written.
    first_unwritten_new_item_index: usize, // 0
    /// Deleted item contents, and the scope of the deletion.
    deleted_items: HashMap<WString, DeletionScope>,
    /// The history file contents.
    file_contents: Option<HistoryFile>,
    /// The file ID of the history file.
    history_file_id: FileId, // INVALID_FILE_ID
    /// How many items we add until the next vacuum. Initially a random value.
    countdown_to_vacuum: Option<usize>,
    /// The boundary timestamp distinguishes old items from new items. Items whose timestamps are <=
    /// the boundary are considered "old". Items whose timestamps are > the boundary are new, and are
    /// ignored by this instance (unless they came from this instance). The timestamp may be adjusted
    /// by incorporate_external_changes().
    boundary_timestamp: SystemTime,
}

impl HistoryImpl {
    /// Returns the canonical path for the history file, or `Ok(None)` in private mode.
    /// An error is returned if obtaining the data directory fails.
    /// Because the `path_get_data` function does not return error information,
    /// we cannot provide more detail about the reason for the failure here.
    fn history_file_path(&self) -> std::io::Result<Option<WString>> {
        if self.cached_name.is_empty() {
            return Ok(None);
        }

        let mut path = if let Some(custom_dir) = &self.custom_directory {
            custom_dir.clone()
        } else if let Some(data_path) = path_get_data() {
            data_path
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Error obtaining data directory. This is a manually constructed error which does not indicate why this happened.",
            ));
        };

        path.push('/');
        path.push_utfstr(&self.cached_name);
        path.push_utfstr(L!("_history"));

        // For custom directories, skip wrealpath since file may not exist yet
        if self.custom_directory.is_some() {
            Ok(Some(path))
        } else if let Some(canonicalized_path) = wrealpath(&path) {
            Ok(Some(canonicalized_path))
        } else {
            Err(std::io::Error::other(format!(
                "wrealpath failed to produce a canonical version of '{path}'."
            )))
        }
    }

    /// Add a new history item to the end.
    fn add(&mut self, mut item: HistoryItem, do_save: bool) {
        // We use empty items as sentinels to indicate the end of history.
        // Do not allow them to be added (#6032).
        if item.is_empty() {
            return;
        }

        // Try merging with the last item.
        if let Some(last) = self.new_items.last_mut() {
            if last.merge(&item) {
                return;
            }
        }

        item.set_timestamp(nudge_timestamp_if_at_boundary(
            item.get_timestamp(),
            self.boundary_timestamp,
        ));

        // We have to add a new item.
        self.new_items.push(item);
        if do_save {
            self.save();
        }
    }

    /// Internal function.
    fn clear_file_state(&mut self) {
        // Erase everything we know about our file.
        self.file_contents = None;
    }

    /// Loads old items if necessary.
    /// Return a reference to the loaded history file.
    fn load_old_if_needed(&mut self) -> &HistoryFile {
        if let Some(ref file_contents) = self.file_contents {
            return file_contents;
        }
        let Ok(Some(history_path)) = self.history_file_path() else {
            return self.file_contents.insert(HistoryFile::create_empty());
        };

        let _profiler = TimeProfiler::new("load_old");
        let file_contents = match lock_and_load(&history_path, RawHistoryFile::create) {
            Ok((file_id, history_file)) => {
                self.history_file_id = file_id;
                let _profiler = TimeProfiler::new("populate_from_file_contents");
                let file_contents = history_file.decode(Some(self.boundary_timestamp));
                flogf!(
                    history,
                    "Loaded %u old items",
                    file_contents.offsets().len()
                );
                file_contents
            }
            Err(e) => {
                flog!(history_file, "Error reading from history file:", e);
                HistoryFile::create_empty()
            }
        };
        self.file_contents.insert(file_contents)
    }

    /// Deletes duplicates in new_items.
    fn compact_new_items(&mut self) {
        // Keep only the most recent items with the given contents.
        let mut seen = HashSet::new();
        for idx in (0..self.new_items.len()).rev() {
            let item = &self.new_items[idx];

            // TODO here we don't need to check anything because we always write to disk
            // Only compact persisted items.
            // if !item.should_write_to_disk() {
            //     continue;
            // }

            if !seen.insert(item.str().to_owned()) {
                // This item was not inserted because it was already in the set, so delete the item at
                // this index.
                self.new_items.remove(idx);

                if idx < self.first_unwritten_new_item_index {
                    // Decrement first_unwritten_new_item_index if we are deleting a previously written
                    // item.
                    self.first_unwritten_new_item_index -= 1;
                }
            }
        }
    }

    /// Given an existing history file, write a new history file to `dst`.
    fn rewrite_to_temporary_file(
        &self,
        existing_file: &File,
        dst: &mut File,
    ) -> std::io::Result<()> {
        // We are reading FROM existing_file and writing TO dst

        // Make an LRU cache to save only the last N elements.

        /// When we rewrite the history, the number of items we keep.
        const HISTORY_SAVE_MAX: NonZeroUsize = NonZeroUsize::new(1024 * 256).unwrap();
        let mut lru = LruCache::new(HISTORY_SAVE_MAX);

        // Read in existing items (which may have changed out from underneath us, so don't trust our
        // old file contents).
        let file_id = file_id_for_file(existing_file);
        if let Ok(local_file) = RawHistoryFile::create(existing_file, file_id) {
            for offset in local_file.offsets(None) {
                // Try decoding an old item.
                let Some(old_item) = local_file.decode_item(offset) else {
                    continue;
                };
                if old_item.is_empty() {
                    continue;
                }

                // Check if this item should be deleted.
                if let Some(&scope) = self.deleted_items.get(old_item.str()) {
                    // If old item is newer than session always erase if in deleted.
                    // If old item is older and in deleted items don't erase if added by clear_session.
                    let delete = old_item.get_timestamp() > self.boundary_timestamp
                        || scope == DeletionScope::AllSessions;
                    if delete {
                        continue;
                    }
                }
                lru.add_item(old_item);
            }
        }

        // Insert any unwritten new items
        for item in self
            .new_items
            .iter()
            .skip(self.first_unwritten_new_item_index)
        {
            lru.add_item(item.clone());
        }

        // Stable-sort our items by timestamp
        // This is because we may have read "old" items with a later timestamp than our "new" items
        // This is the essential step that roughly orders items by history
        let mut items: Vec<_> = lru.into_iter().map(|(_key, item)| item).collect();
        items.sort_by_key(HistoryItem::get_timestamp);

        /// Default buffer size for flushing to the history file.
        const HISTORY_OUTPUT_BUFFER_SIZE: usize = 64 * 1024;
        // Write them out.
        let mut buffer = BufWriter::with_capacity(HISTORY_OUTPUT_BUFFER_SIZE + 128, dst);
        for item in items {
            write_to(&item, &mut buffer)?;
        }
        buffer.flush()?;
        Ok(())
    }

    /// Saves history by rewriting the file.
    fn save_internal_via_rewrite(&mut self, history_path: &wstr) -> std::io::Result<()> {
        flogf!(
            history,
            "Saving %u items via rewrite",
            self.new_items.len() - self.first_unwritten_new_item_index
        );

        let rewrite =
            |old_file: &File, tmp_file: &mut File| -> std::io::Result<PotentialUpdate<()>> {
                let result = self.rewrite_to_temporary_file(old_file, tmp_file);
                if let Err(err) = result {
                    flog!(
                        history_file,
                        "Error writing to temporary history file:",
                        err
                    );
                    return Err(err);
                }
                Ok(PotentialUpdate {
                    do_save: true,
                    data: (),
                })
            };

        let (file_id, _) = rewrite_via_temporary_file(history_path, rewrite)?;
        self.history_file_id = file_id;

        // We've saved everything, so we have no more unsaved items.
        self.first_unwritten_new_item_index = self.new_items.len();

        // We deleted our deleted items.
        self.deleted_items.clear();

        // Our history has been written to the file, so clear our state so we can re-reference the
        // file.
        self.clear_file_state();

        Ok(())
    }

    /// Saves history by appending to the file.
    fn save_internal_via_appending(&mut self, history_path: &wstr) -> std::io::Result<()> {
        flogf!(
            history,
            "Saving %u items via appending",
            self.new_items.len() - self.first_unwritten_new_item_index
        );
        // No deleting allowed.
        assert!(self.deleted_items.is_empty());

        let mut locked_history_file =
            LockedFile::new(LockingMode::Exclusive(WriteMethod::Append), history_path)?;

        // Check if the file was modified since it was last read.
        // If someone has replaced the file, forget our file state.
        if file_id_for_file(locked_history_file.get()) != self.history_file_id {
            self.clear_file_state();
        }

        // We took the exclusive lock. Append to the file.
        // Note that this is sketchy for a few reasons:
        //   - Another shell may have appended its own items with a later timestamp, so our file may
        // no longer be sorted by timestamp.
        //   - Another shell may have appended the same items, so our file may now contain
        // duplicates.
        //
        // Originally we always rewrote the file on saving, which avoided both of these problems.
        // However, appending allows us to save history after every command, which is nice!
        //
        // Periodically we "clean up" the file by rewriting it, so that most of the time it doesn't
        // have duplicates, although we don't yet sort by timestamp (the timestamp isn't really used
        // for much anyways).

        let mut buffer = Vec::new();
        let mut new_first_index = self.first_unwritten_new_item_index;
        while new_first_index < self.new_items.len() {
            let item = &self.new_items[new_first_index];
            // Can't error writing to a buffer.
            write_to(&item, &mut buffer).unwrap();
            // We wrote or skipped this item, hooray.
            new_first_index += 1;
        }
        locked_history_file.get_mut().write_all(&buffer)?;
        fsync(locked_history_file.get())?;
        self.first_unwritten_new_item_index = new_first_index;

        // Since we just modified the file, update our history_file_id to match its current state
        // Otherwise we'll think the file has been changed by someone else the next time we go to
        // write.
        // We don't update `self.file_contents` since we only appended to the file, and everything we
        // appended remains in our new_items
        self.history_file_id = file_id_for_file(locked_history_file.get());

        Ok(())
    }

    /// Save history and vacuum it (commit deleted commands causing a rewrite of the history) if needed.
    fn save_and_vacuum(&mut self, vacuum: bool) {
        // Nothing to do if there's no new items.
        if self.first_unwritten_new_item_index >= self.new_items.len()
            && self.deleted_items.is_empty()
        {
            return;
        }

        // Compact our new items so we don't have duplicates.
        self.compact_new_items();

        if self.cached_name.is_empty() {
            // We're in the "incognito" mode. Pretend we've saved the history.
            self.first_unwritten_new_item_index = self.new_items.len();
            self.deleted_items.clear();
            self.clear_file_state();
            return;
        }

        let history_path = match self.history_file_path() {
            Ok(history_path) => history_path.unwrap(),
            Err(e) => {
                flog!(history, "Saving history failed:", e);
                return;
            }
        };

        // Try saving. If we have items to delete, we have to rewrite the file. If we do not, we can
        // append to it.
        let mut ok = false;
        if !vacuum && self.deleted_items.is_empty() {
            // Try doing a fast append.
            if let Err(e) = self.save_internal_via_appending(&history_path) {
                flog!(history, "Appending to history failed:", e);
            } else {
                ok = true;
            }
        }
        if !ok {
            // We did not or could not append; rewrite the file ("vacuum" it).
            if let Err(e) = self.save_internal_via_rewrite(&history_path) {
                flog!(history, "Rewriting history failed:", e);
            }
        }
    }

    /// Saves history.
    fn save(&mut self) {
        // We may or may not vacuum. We try to vacuum every `VACUUM_FREQUENCY` items, but start the
        // countdown at a random number so that even if the user never runs more than 25 commands, we'll
        // eventually vacuum.  If countdown_to_vacuum is None, it means we haven't yet picked a value for
        // the counter.
        let countdown_to_vacuum = self
            .countdown_to_vacuum
            .get_or_insert_with(|| rand::rng().random_range(0..VACUUM_FREQUENCY));

        // Determine if we're going to vacuum.
        let mut vacuum = false;
        if *countdown_to_vacuum == 0 {
            *countdown_to_vacuum = VACUUM_FREQUENCY;
            vacuum = true;
        }

        // Update our countdown.
        assert!(*countdown_to_vacuum > 0);
        *countdown_to_vacuum -= 1;

        // This might be a good candidate for moving to a background thread.
        let _profiler = TimeProfiler::new(if vacuum {
            "save vacuum"
        } else {
            "save no vacuum"
        });
        self.save_and_vacuum(vacuum);
    }

    fn new(name: WString, custom_directory: Option<WString>) -> Self {
        let mut obj = Self {
            cached_name: name,
            custom_directory,
            new_items: vec![],
            first_unwritten_new_item_index: 0,
            deleted_items: HashMap::new(),
            file_contents: None,
            history_file_id: INVALID_FILE_ID,
            countdown_to_vacuum: None,
            boundary_timestamp: SystemTime::now(),
        };
        obj.populate_from_config_path();
        obj
    }

    /// Determines whether the history is empty. Unfortunately this cannot be const, since it may
    /// require populating the history.
    fn is_empty(&mut self) -> bool {
        // If we have new items, we're not empty.
        if !self.new_items.is_empty() {
            return false;
        }

        if let Some(file_contents) = &self.file_contents {
            // If we've loaded old items, see if we have any offsets.
            file_contents.is_empty()
        } else {
            // If we have not loaded old items, don't actually load them (which may be expensive); just
            // stat the file and see if it exists and is nonempty.

            let Ok(Some(where_)) = self.history_file_path() else {
                return true;
            };

            if let Ok(md) = wstat(&where_) {
                // We're empty if the file is empty.
                md.len() == 0
            } else {
                // Access failed, assume missing.
                true
            }
        }
    }

    /// Remove a history item.
    fn remove(&mut self, str_to_remove: &wstr) {
        // Add to our list of deleted items.
        self.deleted_items
            .insert(str_to_remove.to_owned(), DeletionScope::AllSessions);

        for idx in (0..self.new_items.len()).rev() {
            let matched = self.new_items[idx].str() == str_to_remove;
            if matched {
                self.new_items.remove(idx);
                // If this index is before our first_unwritten_new_item_index, then subtract one from
                // that index so it stays pointing at the same item. If it is equal to or larger, then
                // we have not yet written this item, so we don't have to adjust the index.
                if idx < self.first_unwritten_new_item_index {
                    self.first_unwritten_new_item_index -= 1;
                }
            }
        }
        assert!(self.first_unwritten_new_item_index <= self.new_items.len());
    }

    /// Irreversibly clears history.
    fn clear(&mut self) {
        self.new_items.clear();
        self.deleted_items.clear();
        self.first_unwritten_new_item_index = 0;
        self.file_contents = None;
        if let Ok(Some(filename)) = self.history_file_path() {
            let _ = wunlink(&filename);
        }
        self.clear_file_state();
    }

    /// Clears only session.
    fn clear_session(&mut self) {
        for item in &self.new_items {
            self.deleted_items
                .insert(item.str().to_owned(), DeletionScope::SessionOnly);
        }

        self.new_items.clear();
        self.first_unwritten_new_item_index = 0;
    }

    /// Populates from older location (in config path, rather than data path).
    /// This is accomplished by clearing ourselves, and copying the contents of the old history
    /// file to the new history file.
    /// The new contents will automatically be re-mapped later.
    fn populate_from_config_path(&mut self) {
        let Ok(Some(new_file)) = self.history_file_path() else {
            return;
        };

        let Some(mut old_file) = path_get_config() else {
            return;
        };

        old_file.push('/');
        old_file.push_utfstr(&self.cached_name);
        old_file.push_str("_history");

        let Ok(mut src_file) = wopen_cloexec(&old_file, OFlag::O_RDONLY, Mode::empty()) else {
            return;
        };

        // Clear must come after we've retrieved the new_file name, and before we open
        // destination file descriptor, since it destroys the name and the file.
        self.clear();

        let mut dst_file = match wopen_cloexec(
            &new_file,
            OFlag::O_WRONLY | OFlag::O_CREAT,
            LOCKED_FILE_MODE,
        ) {
            Ok(file) => file,
            Err(err) => {
                flog!(history_file, "Error when writing history file:", err);
                return;
            }
        };

        let mut buf = [0; libc::BUFSIZ as usize];
        while let Ok(n) = src_file.read(&mut buf) {
            if n == 0 {
                break;
            }

            if let Err(err) = dst_file.write_all(&buf[..n]) {
                flog!(history_file, "Error when writing history file:", err);
                break;
            }
        }
    }

    /// Incorporates the history of other shells into this history.
    fn incorporate_external_changes(&mut self) {
        // To incorporate new items, we simply update our timestamp to now, so that items from previous
        // instances get added. We then clear the file state so that we remap the file. Note that this
        // is somewhat expensive because we will be going back over old items. An optimization would be
        // to preserve old_item_offsets so that they don't have to be recomputed. (However, then items
        // *deleted* in other instances would not show up here).
        let new_timestamp = SystemTime::now();

        // If for some reason the clock went backwards, we don't want to start dropping items; therefore
        // we only do work if time has progressed. This also makes multiple calls cheap.
        if new_timestamp > self.boundary_timestamp {
            self.boundary_timestamp = new_timestamp;
            self.clear_file_state();

            // We also need to erase new items, since we go through those first, and that means we
            // will not properly interleave them with items from other instances.
            // We'll pick them up from the file (#2312)
            // TODO: this will drop items that had no_persist set, how can we avoid that while still
            // properly interleaving?
            self.save_and_vacuum(false);
            self.new_items.clear();
            self.first_unwritten_new_item_index = 0;
        }
    }

    // TODO redesign required paths
    /// Sets the valid file paths for the history item matching the snapshotted item.
    // fn set_valid_file_paths(&mut self, valid_file_paths: Vec<WString>, snapshot: &HistoryItem) {
    //     // Look for an item with the given identifier. It is likely to be at the end of new_items.
    //     for item in self.new_items.iter_mut().rev() {
    //         if item.get_timestamp() == snapshot.get_timestamp() && item.str() == snapshot.str() {
    //             // found it
    //             item.set_required_paths(valid_file_paths);
    //             break;
    //         }
    //     }
    // }

    /// Return the specified history at the specified index. 0 is the index of the current
    /// commandline. (So the most recent item is at index 1.)
    fn item_at_index(&mut self, mut idx: usize) -> Option<Cow<'_, HistoryItem>> {
        // 0 is considered an invalid index.
        if idx == 0 {
            return None;
        }
        idx -= 1;

        // Determine how many "resolved" (non-pending) items we have. We can have at most one pending
        // item, and it's always the last one.
        let resolved_new_item_count = self.new_items.len();

        // TODO No more logic regarding pending items
        // if self.has_pending_item && resolved_new_item_count > 0 {
        //     resolved_new_item_count -= 1;
        // }

        // idx == 0 corresponds to the last resolved item.
        if idx < resolved_new_item_count {
            return Some(Cow::Borrowed(
                &self.new_items[resolved_new_item_count - idx - 1],
            ));
        }

        // Now look in our old items.
        idx -= resolved_new_item_count;
        let file_contents = self.load_old_if_needed();
        let old_item_offsets = file_contents.offsets();
        let old_item_count = old_item_offsets.len();
        if idx < old_item_count {
            // idx == 0 corresponds to last item in old_item_offsets.
            let offset = old_item_offsets[old_item_count - idx - 1];
            return file_contents.decode_item(offset).map(Cow::Owned);
        }

        // Index past the valid range, so return None.
        None
    }

    /// Return the number of history entries.
    fn size(&mut self) -> usize {
        let new_item_count = self.new_items.len();
        let old_item_offsets = self.load_old_if_needed().offsets();
        new_item_count + old_item_offsets.len()
    }
}

/// Big hack: do not allow timestamps equal to our boundary date. This is because we include
/// items whose timestamps are equal to our boundary when reading old history, so we can catch
/// "just closed" items. But this means that we may interpret our own items, that we just wrote,
/// as old items, if we wrote them in the same second as our birthdate.
fn nudge_timestamp_if_at_boundary(mut when: SystemTime, boundary: SystemTime) -> SystemTime {
    if when.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
        == boundary
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    {
        when += Duration::from_secs(1);
    }
    when
}

pub struct YAMLHistory {
    /// The name of this list. Used for picking a suitable filename and for switching modes.
    name: WString,
    inner: Mutex<HistoryImpl>,
}

impl YAMLHistory {
    fn imp(&self) -> MutexGuard<'_, HistoryImpl> {
        self.inner.lock().unwrap()
    }

    /// Creates a new History with a custom directory path.
    /// The history file will be stored at `{directory}/{name}_history`.
    /// If the directory is None, it will be stored at path_get_data().
    pub fn new(name: &wstr, directory: Option<WString>) -> Self {
        Self {
            name: name.to_owned(),
            inner: Mutex::new(HistoryImpl::new(name.to_owned(), directory)),
        }
    }

    /// Irreversibly clears history for the current session.
    pub fn clear_session(&self) {
        self.imp().clear_session();
    }

    /// Populates from older location (in config path, rather than data path).
    pub fn populate_from_config_path(&self) {
        self.imp().populate_from_config_path();
    }

    /// Incorporates the history of other shells into this history.
    pub fn incorporate_external_changes(&self) {
        self.imp().incorporate_external_changes();
    }
}

/// We use the default implementation for get_history
impl HistoryProvider for YAMLHistory {
    /// Name of the history.
    fn name(&self) -> &wstr {
        &self.name
    }

    /// Return the specified history at the specified index. 0 is the index of the current
    /// commandline. (So the most recent item is at index 1.)
    fn item_at_index(&self, idx: usize) -> Option<fish_history_api::HistoryItem> {
        self.imp().item_at_index(idx).map(Cow::into_owned)
    }

    /// Add an item.
    fn add(&self, item: HistoryItem) {
        self.imp().add(item, true);
    }

    /// Remove a history item.
    fn remove(&self, s: &wstr) {
        self.imp().remove(s);
    }

    /// Irreversibly clears history.
    fn clear(&self) {
        self.imp().clear();
    }

    /// Return the number of history entries.
    fn size(&self) -> u64 {
        self.imp().size() as u64
    }

    /// Saves history with automatic "vacuuming".
    fn save(&self) {
        self.imp().save();
    }

    /// Determines whether the history is empty.
    fn is_empty(&self) -> bool {
        self.imp().is_empty()
    }
}
