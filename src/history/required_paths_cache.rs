//! We provide an in-memory cache that maps commands to vectors of absolute paths.
//! - This mapping is populated when a command is executed and the arguments which "look like paths" are checked (via `[History::spawn_file_detection`]).
//! - The entries in this map are not updated once they have been created.
//! - When the user selects a command from the history, if we have a cache hit, then we only check those absolute paths.
//!
//! When the user runs a command `ls rel /abs` we detect that `rel` and `/abs` look like paths and spawn background tasks to stat them.
//! The background task then populates the cache with the command string and a vector of valid paths.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use crate::prelude::*;

static REQUIRED_PATHS_CACHE: LazyLock<Mutex<HashMap<WString, Vec<WString>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn insert(s: WString, paths: Vec<WString>) {
    let mut cache = REQUIRED_PATHS_CACHE.lock().unwrap();
    cache.insert(s, paths);
}

pub fn get(s: &wstr) -> Option<Vec<WString>> {
    let cache = REQUIRED_PATHS_CACHE.lock().unwrap();
    cache.get(s).cloned()
}
