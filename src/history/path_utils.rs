use crate::{
    env::Environment,
    expand::{ExpandFlags, expand_one},
    operation_context::{EXPANSION_LIMIT_BACKGROUND, OperationContext},
    path::path_is_valid,
    prelude::*,
    threads::assert_is_background_thread,
};

/// Given a list of proposed paths and a context, perform variable and home directory expansion,
/// and detect if the result expands to a value which is also the path to a file.
/// Wildcard expansions are suppressed - see implementation comments for why.
///
/// This is used for autosuggestion hinting. If we add an item to history, and one of its arguments
/// refers to a file, then we only want to suggest it if there is a valid file there.
/// This does disk I/O and may only be called in a background thread.
pub fn expand_and_detect_paths<P: IntoIterator<Item = WString>>(
    paths: P,
    vars: &dyn Environment,
) -> Vec<WString> {
    assert_is_background_thread();
    let working_directory = vars.get_pwd_slash();
    let ctx = OperationContext::background(vars, EXPANSION_LIMIT_BACKGROUND);
    let mut result = Vec::new();
    for path in paths {
        // Suppress cmdsubs since we are on a background thread and don't want to execute fish
        // script.
        // Suppress wildcards because we want to suggest e.g. `rm *` even if the directory
        // is empty (and so rm will fail); this is nevertheless a useful command because it
        // confirms the directory is empty.
        let mut expanded_path = path.clone();
        if expand_one(
            &mut expanded_path,
            ExpandFlags::FAIL_ON_CMDSUBST | ExpandFlags::SKIP_WILDCARDS,
            &ctx,
            None,
        ) && path_is_valid(&expanded_path, &working_directory)
        {
            // Note we return the original (unexpanded) path.
            result.push(path);
        }
    }

    result
}

/// Given a list of proposed paths and a context, expand each one and see if it refers to a file.
/// Wildcard expansions are suppressed.
/// Returns `true` if `paths` is empty or every path is valid.
pub fn all_paths_are_valid(paths: &[WString], ctx: &OperationContext<'_>) -> bool {
    assert_is_background_thread();
    let working_directory = ctx.vars().get_pwd_slash();
    let mut path = WString::new();
    for unexpanded_path in paths {
        path.clone_from(unexpanded_path);
        if ctx.check_cancel() {
            return false;
        }
        if !expand_one(
            &mut path,
            ExpandFlags::FAIL_ON_CMDSUBST | ExpandFlags::SKIP_WILDCARDS,
            ctx,
            None,
        ) {
            return false;
        }
        if !path_is_valid(&path, &working_directory) {
            return false;
        }
    }
    true
}
