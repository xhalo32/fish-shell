use crate::{
    ast::{self, Kind, Node},
    env::{EnvMode, EnvStack},
    history::{
        HistoryItem, LocalHistoryItem,
        external::Provider,
        format::format_history_record,
        path_utils::expand_and_detect_paths,
        required_paths_cache::insert,
        search::{SearchType, do_1_history_search},
    },
    io::IoStreams,
    parse_constants::{ParseTreeFlags, StatementDecoration},
    parser::Parser,
    threads::ThreadPool,
};
use fish_history_api::HistoryProvider;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::ControlFlow,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use crate::{
    common::{CancelChecker, UnescapeStringStyle, unescape_string, valid_var_name},
    env::{EnvSetMode, EnvVar, Environment},
    flog::flog,
    localization::wgettext_fmt,
    prelude::*,
};

/// This is the history session ID we use by default if the user has not set env var fish_history.
pub const DFLT_FISH_HISTORY_SESSION_ID: &wstr = L!("fish");

pub type PathList = Vec<WString>;

/// Find all arguments that look like they could be file paths.
/// Returns `None` if the command is likely to trigger an exit.
fn find_potential_paths(s: &wstr, ast: &ast::Ast) -> Option<Vec<WString>> {
    // TODO relocate
    fn string_could_be_path(potential_path: &wstr) -> bool {
        // Assume that things with leading dashes aren't paths.
        !(potential_path.is_empty() || potential_path.starts_with('-'))
    }

    let mut potential_paths = Vec::new();
    for node in ast.walk() {
        if let Kind::Argument(arg) = node.kind() {
            let potential_path = arg.source(s);
            if string_could_be_path(potential_path) {
                potential_paths.push(potential_path.to_owned());
            }
        } else if let Kind::DecoratedStatement(stmt) = node.kind() {
            // Hack hack hack - if the command is likely to trigger an exit, then don't do
            // background file detection, because we won't be able to write it to our history file
            // before we exit.
            // Also skip it for 'echo'. This is because echo doesn't take file paths, but also
            // because the history file test wants to find the commands in the history file
            // immediately after running them, so it can't tolerate the asynchronous file detection.
            if stmt.decoration() == StatementDecoration::Exec {
                return None;
            }

            let source = stmt.command.source(s);
            let command = unescape_string(source, UnescapeStringStyle::default());
            let command = command.as_deref().unwrap_or(source);
            if [L!("exit"), L!("reboot"), L!("restart"), L!("echo")].contains(&command) {
                return None;
            }
        }
    }
    Some(potential_paths)
}

/// A history containing local and shared history items.
/// The shared history is provided by a [`HistoryProvider`].
pub struct History<P: HistoryProvider> {
    /// Local list of history items which contain a [`PersistenceMode`].
    local: Mutex<Vec<LocalHistoryItem>>,
    /// The shared history provider.
    pub(super) provider: P,
    /// Thread pool for background operations.
    thread_pool: Arc<ThreadPool>,
}

impl<P: HistoryProvider> History<P> {
    pub fn new(provider: P) -> Self {
        Self {
            local: Mutex::new(Vec::new()),
            provider,
            // Up to 8 threads, no soft min.
            thread_pool: ThreadPool::new(0, 8),
        }
    }

    pub fn name(&self) -> &wstr {
        &self.provider.name()
    }

    pub fn item_at_index(&self, idx: usize) -> Option<HistoryItem> {
        if idx == 0 {
            return None;
        }
        let local = self.local.lock().unwrap();
        if idx <= local.len() {
            // Comes from the local history
            return Some(HistoryItem::LocalHistoryItem(
                local[local.len() - idx].clone(),
            ));
        } else {
            // Comes from the shared history provider
            Some(HistoryItem::SharedHistoryItem(
                self.provider.item_at_index(idx - local.len())?,
            ))
        }
    }

    /// Returns whether this is using the default name.
    pub fn is_default(&self) -> bool {
        self.name() == DFLT_FISH_HISTORY_SESSION_ID
    }

    /// Is the provider empty. This is only used by bash history import.
    /// We don't want to import bash history even if the provider reports size == 0.
    pub fn is_empty(&self) -> bool {
        self.provider.is_empty()
    }

    fn local_items(&self) -> MutexGuard<'_, Vec<LocalHistoryItem>> {
        self.local.lock().unwrap()
    }

    /// Removes trailing ephemeral items.
    /// Ephemeral items have leading spaces, and can only be retrieved immediately; adding any item
    /// removes them.
    pub fn remove_ephemeral_items(&self) {
        let mut local = self.local_items();
        while let Some(last) = local.last()
            && last.is_ephemeral()
        {
            local.pop();
        }
    }

    /// Gets all the history into a list. This is intended for the $history environment variable.
    /// This may be long!
    pub fn get_history(&self) -> Vec<WString> {
        let mut result = vec![];
        let mut seen = HashSet::new();

        let shared_iter = self.provider.get_history();
        for item in self
            .local_items()
            .iter()
            .map(|item| item.str())
            .rev()
            .chain(shared_iter.iter().map(|item| item.str()))
        {
            if seen.insert(item) {
                result.push(item.to_owned());
            }
        }
        result
    }

    /// Let indexes be a list of one-based indexes into the history, matching the interpretation of
    /// `$history`. That is, `$history[1]` is the most recently executed command. Values less than one
    /// are skipped. Return a mapping from index to history item text.
    pub fn items_at_indexes(
        &self,
        indexes: impl IntoIterator<Item = usize>,
    ) -> HashMap<usize, WString> {
        let mut result = HashMap::new();
        for idx in indexes {
            // If this is the first time the index is encountered, we have to go fetch the item.
            #[allow(clippy::map_entry)] // looks worse
            if !result.contains_key(&idx) {
                // New key.
                let contents = match self.item_at_index(idx) {
                    Some(item) => item.into_str(),
                    None => WString::new(),
                };
                result.insert(idx, contents);
            }
        }
        result
    }

    pub fn size(&self) -> usize {
        self.local_items().len() + self.provider.size() as usize
    }

    /// Saves history.
    pub fn save(&self) {
        self.provider.save();
    }

    /// Removes all occurrences of s from local and shared history
    pub fn remove(&self, s: &wstr) {
        self.local_items().retain(|item| item.str() == s);
        self.provider.remove(s);
    }

    pub fn clear(&self) {
        self.local_items().clear();
        self.provider.clear();
    }

    /// Gives direct access to the provider. Used by builtins and for testing.
    // pub fn provider(&self) -> &P {
    //     &self.provider
    // }

    /// Add a new shared history item. The item is sent to the history provider to store.
    pub fn add_shared(&self, s: WString, time: SystemTime, vars: &EnvStack) {
        let item = fish_history_api::HistoryItem::new(s, time);

        self.spawn_file_detection(item.str(), vars);
        self.provider.add(item);
    }

    /// Add a new local history item. The item is not sent to the history provider.
    pub fn add_local(&self, item: LocalHistoryItem, vars: &EnvStack) {
        // We use empty items as sentinels to indicate the end of history.
        // Do not allow them to be added (#6032).
        if item.is_empty() {
            return;
        }

        // Try merging with the last item.
        if let Some(last) = self.local_items().last_mut() {
            if last.merge(&item) {
                // We merged, so we don't have to file detection.
                return;
            }
        }

        // Don't bother doing file detection for ephemeral items
        if !item.is_ephemeral() {
            self.spawn_file_detection(item.str(), vars);
        }
        self.local_items().push(item.into());
    }

    /// Begin file detection on the items to determine which arguments are paths.
    /// Arguments may be expanded (e.g. with PWD and variables)
    /// using the given `vars`.
    fn spawn_file_detection(&self, s: &wstr, vars: &EnvStack) {
        let ast = ast::parse(s, ParseTreeFlags::default(), None);

        // If we got a path, we'll perform file detection for autosuggestion hinting.
        if let Some(potential_paths) = find_potential_paths(s, &ast) {
            if potential_paths.is_empty() {
                return;
            }
            // Check for which paths are valid on a background thread.
            // Don't hold the lock while we perform this file detection.
            let thread_pool = Arc::clone(&self.thread_pool);
            let vars_snapshot = vars.snapshot();
            let s = s.to_owned();
            thread_pool.perform(move || {
                let valid_file_paths = expand_and_detect_paths(potential_paths, &vars_snapshot);
                if !valid_file_paths.is_empty() {
                    insert(s, valid_file_paths);
                }
            });
        } else {
            // We're about to exit, save immediately, regardless of any disabling. This may
            // cause us to lose file hinting for some commands, but it beats losing history items.
            // FIXME this may vacuum, does the API need to provide "emergency save"?
            self.provider.save();
        }
    }

    /// Add a shared item through the commandline where file detection is unnecessary.
    pub fn add_shared_no_file_detection(&self, s: WString, time: SystemTime) {
        let item = fish_history_api::HistoryItem::new(s, time);
        self.provider.add(item);
    }

    /// Searches history.
    #[allow(clippy::too_many_arguments)]
    pub fn search(
        self: &Arc<Self>,
        parser: &Parser,
        streams: &mut IoStreams,
        search_type: SearchType,
        search_args: &[&wstr],
        show_time_format: Option<&str>,
        max_items: usize,
        case_sensitive: bool,
        null_terminate: bool,
        reverse: bool,
        cancel_check: &CancelChecker,
        color_enabled: bool,
    ) -> bool {
        let mut remaining = max_items;
        let mut collected = Vec::new();
        let mut output_error = false;

        // The function we use to act on each item.
        let mut func = |item: &HistoryItem| {
            if remaining == 0 {
                return ControlFlow::Break(());
            }
            remaining -= 1;
            let formatted_record = format_history_record(
                item,
                show_time_format,
                null_terminate,
                parser,
                color_enabled,
            );

            if reverse {
                // We need to collect this for later.
                collected.push(formatted_record);
            } else {
                // We can output this immediately.
                if !streams.out.append(&formatted_record) {
                    // This can happen if the user hit Ctrl-C to abort (maybe after the first page?).
                    output_error = true;
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        };

        if search_args.is_empty() {
            // The user had no search terms; just append everything.
            do_1_history_search(
                Arc::clone(self),
                SearchType::Contains,
                WString::new(),
                true,
                &mut func,
                cancel_check,
            );
        } else {
            #[allow(clippy::unnecessary_to_owned)]
            for search_string in search_args.iter().copied() {
                if search_string.is_empty() {
                    streams
                        .err
                        .append(L!("Searching for the empty string isn't allowed"));
                    return false;
                }
                do_1_history_search(
                    Arc::clone(self),
                    search_type,
                    search_string.to_owned(),
                    case_sensitive,
                    &mut func,
                    cancel_check,
                );
            }
        }

        // Output any items we collected (which only happens in reverse).
        for item in collected.into_iter().rev() {
            if output_error {
                break;
            }

            if !streams.out.append(&item) {
                // Don't force an error if output was aborted (typically via Ctrl-C/SIGINT); just don't
                // try writing any more.
                output_error = true;
            }
        }

        // We are intentionally not returning false in case of an output error, as the user aborting the
        // output early (the most common case) isn't a reason to exit w/ a non-zero status code.
        true
    }
}

static HISTORIES: Mutex<BTreeMap<WString, Arc<History<Provider>>>> = Mutex::new(BTreeMap::new());

/// Returns the history with the given name, creating it if necessary, using the default data directory.
/// This uses the HISTORIES global collection. Note it is possible to create a history without
/// placing it into this collection.
pub fn with_name(name: &wstr) -> Arc<History<Provider>> {
    let mut histories = HISTORIES.lock().unwrap();

    if let Some(hist) = histories.get(name) {
        Arc::clone(hist)
    } else {
        let provider = Provider::new(name, None);
        let hist = Arc::new(History::<Provider>::new(provider));
        histories.insert(name.to_owned(), Arc::clone(&hist));
        hist
    }
}

/// Saves the new history to disk.
pub fn save_all() {
    for hist in HISTORIES.lock().unwrap().values() {
        hist.save();
    }
}

/// Return the prefix for the files to be used for command and read history.
pub fn history_session_id(vars: &dyn Environment) -> WString {
    history_session_id_from_var(vars.get(L!("fish_history")))
}

pub fn history_session_id_from_var(history_name_var: Option<EnvVar>) -> WString {
    let Some(var) = history_name_var else {
        return DFLT_FISH_HISTORY_SESSION_ID.to_owned();
    };
    let session_id = var.as_string();
    if session_id.is_empty() || valid_var_name(&session_id) {
        session_id
    } else {
        flog!(
            error,
            wgettext_fmt!(
                "History session ID '%s' is not a valid variable name. Falling back to `%s`.",
                &session_id,
                DFLT_FISH_HISTORY_SESSION_ID
            ),
        );
        DFLT_FISH_HISTORY_SESSION_ID.to_owned()
    }
}

/// Sets private mode on. Once in private mode, it cannot be turned off.
pub fn start_private_mode(vars: &EnvStack) {
    let global_mode = EnvSetMode::new_at_early_startup(EnvMode::GLOBAL);
    vars.set_one(L!("fish_history"), global_mode, L!("").to_owned());
    vars.set_one(L!("fish_private_mode"), global_mode, L!("1").to_owned());
}

/// Queries private mode status.
pub fn in_private_mode(vars: &dyn Environment) -> bool {
    vars.get_unless_empty(L!("fish_private_mode")).is_some()
}
