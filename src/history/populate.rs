use crate::{
    ast::{self},
    common::bytes2wcstring,
    history::History,
    parse_constants::ParseTreeFlags,
    parse_util::detect_parse_errors,
};
use fish_history_api::HistoryProvider;
use fish_wcstringutil::trim;
use fish_widestring::subslice_position;
use std::{
    io::BufRead,
    time::SystemTime,
};

use crate::prelude::*;

impl<P: HistoryProvider> History<P> {
    pub fn populate_from_bash<R: BufRead>(&self, contents: R) {
        // Process the entire history file until EOF is observed.
        // Pretend all items were created at this time.
        let when = SystemTime::now();
        for line in contents.split(b'\n') {
            let Ok(line) = line else {
                break;
            };
            let wide_line = trim(bytes2wcstring(&line), None);
            // Add this line if it doesn't contain anything we know we can't handle.
            if should_import_bash_history_line(&wide_line) {
                let item = fish_history_api::HistoryItem::new(wide_line, when);
                self.provider.add(item);
            }
        }
    }
}

fn should_import_bash_history_line(line: &wstr) -> bool {
    if line.is_empty() {
        return false;
    }

    // The following are Very naive tests!

    // Skip comments.
    if line.starts_with('#') {
        return false;
    }

    // Skip lines with backticks because we don't have that syntax,
    // Skip brace expansions and globs because they don't work like ours
    // Skip lines that end with a backslash. We do not handle multiline commands from bash history.
    if line.chars().any(|c| matches!(c, '`' | '{' | '*' | '\\')) {
        return false;
    }

    // Skip lines with [[...]] and ((...)) since we don't handle those constructs.
    // "<<" here is a proxy for heredocs (and herestrings).
    for seq in [L!("[["), L!("]]"), L!("(("), L!("))"), L!("<<")] {
        if subslice_position(line.as_char_slice(), seq).is_some() {
            return false;
        }
    }

    if ast::parse(line, ParseTreeFlags::default(), None).errored() {
        return false;
    }

    // In doing this test do not allow incomplete strings. Hence the "false" argument.
    let mut errors = Vec::new();
    let _ = detect_parse_errors(line, Some(&mut errors), false);
    errors.is_empty()
}
