//! dimax: a tmux-style client/server terminal multiplexer.
//! See docs/superpowers/specs/2026-07-30-dimax-design.md for the design.

pub mod cli;
pub mod daemon;
pub mod protocol;
pub mod skills_setup;
pub mod term;
pub mod tui;

/// Shared by every test, in any module, that fakes `$XDG_CONFIG_HOME`/
/// `$HOME` to isolate on-disk config reads/writes (`daemon::pinned_dirs`,
/// `daemon::session_persistence`, `tui::mod`'s pin-config tests) from
/// the real ones. These env vars are process-global, and `cargo test`
/// runs every test in this crate's one test binary concurrently by
/// default -- a lock private to just one of these modules only
/// serializes that module's *own* tests against each other, not
/// against a different module's tests touching the exact same env
/// vars at the same time. One lock, shared by all of them, is what
/// actually prevents that race. `tokio::sync::Mutex`, not `std::sync`,
/// because at least one caller (`tui::mod`'s pin tests) holds the
/// guard across `.await` points; synchronous (`#[test]`, not
/// `#[tokio::test]`) callers use `blocking_lock()` instead.
#[cfg(test)]
pub(crate) static ENV_FAKE_HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
