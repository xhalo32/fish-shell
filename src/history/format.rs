use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::{bytes2wcstring, cstr2wcstring};
use crate::highlight::highlight_and_colorize;
use crate::history::HistoryItem;
use crate::parser::Parser;
use crate::prelude::*;

pub fn time_to_seconds(ts: SystemTime) -> i64 {
    match ts.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            // after epoch
            i64::try_from(d.as_secs()).unwrap()
        }
        Err(e) => {
            // before epoch
            -i64::try_from(e.duration().as_secs()).unwrap()
        }
    }
}

/// Formats a single history record, including a trailing newline.
pub fn format_history_record(
    item: &HistoryItem,
    show_time_format: Option<&str>,
    null_terminate: bool,
    parser: &Parser,
    color_enabled: bool,
) -> WString {
    let mut result = WString::new();
    let seconds = time_to_seconds(item.get_timestamp());
    // This warns for musl, but the warning is useless to us - there is nothing we can or should do.
    #[allow(deprecated)]
    let seconds = seconds as libc::time_t;
    let mut timestamp = MaybeUninit::uninit();
    if let Some(show_time_format) = show_time_format.and_then(|s| CString::new(s).ok()) {
        if !unsafe { libc::localtime_r(&seconds, timestamp.as_mut_ptr()).is_null() } {
            const MAX_TIMESTAMP_LENGTH: usize = 100;
            let mut timestamp_str = [0_u8; MAX_TIMESTAMP_LENGTH];
            if unsafe {
                libc::strftime(
                    timestamp_str.as_mut_ptr().cast(),
                    MAX_TIMESTAMP_LENGTH,
                    show_time_format.as_ptr(),
                    timestamp.as_ptr(),
                )
            } != 0
            {
                // SAFETY: strftime terminates the string with a null byte. If there is insufficient
                // space, strftime returns 0.
                let timestamp_cstr = CStr::from_bytes_until_nul(&timestamp_str).unwrap();
                result.push_utfstr(&cstr2wcstring(timestamp_cstr));
            }
        }
    }

    let mut command = item.str().to_owned();
    if color_enabled {
        command = bytes2wcstring(&highlight_and_colorize(
            &command,
            &parser.context(),
            parser.vars(),
        ));
    }

    result.push_utfstr(&command);
    result.push(if null_terminate { '\0' } else { '\n' });
    result
}
