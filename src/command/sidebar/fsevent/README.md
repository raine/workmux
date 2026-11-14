# macOS watcher backport

`mod.rs` contains the FSEvents backend from the published `notify` crate
**9.0.0-rc.3**, licensed under CC0-1.0 (see `LICENSE-CC0`), with the shutdown fix
from [upstream commit 5ebf4c0ba4e51ffae0a1d73476f9de9f505a82d8](https://github.com/notify-rs/notify/commit/5ebf4c0ba4e51ffae0a1d73476f9de9f505a82d8).

`CFRunLoopStop` issued before run-loop entry can be lost. Bounded run-loop calls
recheck the shutdown flag so watch changes and watcher destruction do not wait
indefinitely for an idle run loop. Git and config watchers both use this backend
on macOS; other platforms use `notify::RecommendedWatcher`.

The backend is compiled into workmux so it is included in crates.io packages as
well as source builds. Cargo removes `[patch]` overrides when publishing.

## Differences from upstream

- Apply the bounded run-loop shutdown fix.
- Import public event types and the `Watcher` trait from `notify`.
- Use standard-library channels (the upstream default), expand the private
  `RecursiveMode::is_recursive` helper, and copy the private `update_paths`
  helper from upstream `src/lib.rs`.
- Add explicit unsafe blocks for Rust 2024 and retain upstream structure with
  local lint allowances.
- Keep regression coverage in the sidebar daemon tests rather than copying
  upstream's crate-internal test harness.

Run `cargo test git_watch_churn_delivers_events_and_stops` on macOS to exercise
100 sibling watch/unwatch cycles, mutation delivery, and destruction in a bounded
child. Churn exercises the race probabilistically; its deadline also bounds
failures in registration and destruction.

For extended stress coverage, run
`cargo test git_watch_churn_stress_delivers_events_and_stops -- --ignored`.
This performs 10,000 cycles with a 15-minute child deadline. It is excluded from
routine checks because FSEvents stream restarts can take several minutes in
aggregate even when shutdown is working. Progress is included in failure output
to distinguish slow churn from a stalled watcher.

Remove this module and its direct FFI dependencies when adopting an upstream
release with suitable registration-failure handling. rc.6 contains the shutdown
fix but also introduces a shared FSEvents path budget: a new registration can
stop the existing stream and then fail. That requires watcher-wide fallback,
not just marking the new root as incompletely watched.
