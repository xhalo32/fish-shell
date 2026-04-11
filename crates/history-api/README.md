# History API

**Summary of changes:**
- A new crate `fish_history_api` declares a `HistoryProvider` trait and a `HistoryItem` struct. This API enables the user to compile fish with a different history provider.
- Local history is handled by `History<P>`, whereas the history provider provides shared history.
- `PersistenceMode` now only has `Memory` and `Ephemeral` and is only used for local history items. The API doesn't use `PersistenceMode` at all.
- The default YAML history has not been extracted to its own crate as it is tightly coupled with other modules. It implements `HistoryProvider` and is the default history provider.

**Key design choices:**
- The `History` struct takes a type parameter `P: HistoryProvider`.
    - Parametricity (API boundaries) is only enforced in `history/`, outside it we use the concrete type exposed via `history::external::Provider`.
    - The `external-history` feature flag makes `history/external.rs` expose the history provider from a dependency named `history_impl` which can be overridden in Cargo.toml.
- Replaced persistent `required_paths` with an in-memory cache. This removes the need for tracking pending history items

## Goals

- Support different shared history providers for fish
- Compile-time selection of history provider
- The current `History` struct is roughly the default history provider. This is refactored to history-yaml
- The history has a clean split into local and shared components. Local behavior (PersistenceMode::Memory and Ephemeral) remains while PersistenceMode::Disk is implemented by the provider

## HistoryProvider API

The API is served as its own crate according to dependency inversion principles.
The API takes most methods from the `History` struct that encapsulates `HistoryImpl`.

- `item_at_index`: Return the specified history at the specified index.
- `clear` destroys the entire history with an interactive warning "your entire interactive command history will be erased"
- TODO (`clear_session`)
- `add` adds an item to the history
- `remove` removes an item from the history. The provider can freely choose to remove all instances, something more specific, or do nothing.
- `size` tells the number of items available to `item_at_index`.
    If `size` returns a number n, then `item_at_index(k)` should get an item where 1 <= k <= n.
    Uses `u64` in favor of `usize` to keep the API CPU architecture independent.
- `is_empty` says if there is no history data in the entire database. May be different from size == 0.
NEW:
- `init`: runs when the provider is initialized. Useful for database migrations etc

API that can't be enforced with traits:
- The implementation must expose a type impl::Provider that implements the HistoryProvider
- The provider type must implement `fn new(&self, session_name: WString, data_path: Option<WString>) -> Self;`

## Unresolved questions

- Should the API communicate the current working directory, or is it enough for the impl to use std::env::current_dir?
- Should we get rid of `items_at_indexes` or change it to a slice rather than a HashMap?
- The `history` built-in could become a general purpose tool for the API, not just the YAML impl
- The deleted items approach seems unnecessarily complex with the new API
- How should the parser skip extra keys in YAML (such as paths)
- current flog is still tightly coupled with the yaml history
- session handling logic and clear_session
- How to add error handling so that providers don't need to panic? `Error` associated type which has to implement `std::error::Error` might be the way to go.

# TODOs

- some tests, bash and other history imports have been removed for the time of the refactoring
- A lot of comments have become outdated


# Problems

- why does save get called so many times at startup?
    - seems to be caused by bash history import
