# Default Attach, Startup Picker, and `dimux config` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bare `dimux` launches the TUI; an empty-workspace attach shows the server-pane picker (except the very first attach against a fresh daemon, which spawns a default shell); `dimux config` opens the generated Kitty chord config in `$EDITOR`.

**Architecture:** `main.rs` wraps `Cli` in an `Args { command: Option<Cli> }` struct so bare invocation defaults to `Cli::Attach`. A new ephemeral `State::used_shell_fallback` flag plus `Request::ConsumeShellFallback`/`Response::ShellFallback` let `App::bootstrap` decide, once, whether to auto-spawn a shell or open the picker on an empty workspace. `confirm_attach_menu` gains a third branch (`ClientSpawn`) for when there's no `self.focused` pane to bind into. `kitty_setup::ensure_installed`'s write logic is split out into a fallible `ensure_config_written` that both the existing silent auto-install and the new `dimux config` subcommand call.

**Tech Stack:** Rust, tokio, serde, clap 4, ratatui

---

### Task 1: `main.rs`/`cli.rs` — bare `dimux` defaults to attach, add `dimux config`

**Files:**
- Modify: `src/main.rs`
- Modify: `src/cli.rs:53-71` (the `Cli` enum), `src/cli.rs:193-204` (`run`)
- Test: `src/cli.rs` (new tests in its existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test for `Args` defaulting to `Cli::Attach`**

Add to `src/cli.rs`'s test module (near `parse_pane_addr_valid`):

```rust
#[test]
fn bare_invocation_defaults_to_attach() {
    let args = Args::try_parse_from(["dimux"]).unwrap();
    assert!(matches!(args.command, Some(Cli::Attach) | None));
}

#[test]
fn config_subcommand_parses() {
    let args = Args::try_parse_from(["dimux", "config"]).unwrap();
    assert!(matches!(args.command, Some(Cli::Config)));
}
```

Run: `cargo check 2>&1 | head -30`
Expected: compile errors — `Args` doesn't exist yet, `Cli::Config` doesn't exist yet, and `try_parse_from` is unresolved because the `clap::Parser` trait is not in scope. Note: `use super::*;` does NOT bring it in, since `cli.rs` only ever names clap fully-qualified in `#[derive(clap::Parser)]`. Add an explicit `use clap::Parser;` to the test module.

- [ ] **Step 2: Change `Cli` from a `Parser` to a `Subcommand`, add `Config`, add the `Args` wrapper**

Replace `src/cli.rs:51-71`:

```rust
/// Top-level CLI argument tree. `main.rs` parses [`Args`] (whose
/// `command` defaults to [`Cli::Attach`] when omitted) and dispatches
/// to [`run`].
#[derive(clap::Subcommand)]
pub enum Cli {
    /// Launch the TUI, attaching to (and auto-starting) the daemon.
    Attach,
    /// Control server-panes.
    Server {
        #[command(subcommand)]
        cmd: ServerCmd,
    },
    /// Control client-panes / workspaces.
    Client {
        #[command(subcommand)]
        cmd: ClientCmd,
    },
    /// Run the daemon in the foreground (used internally by the
    /// auto-spawn path; also useful for debugging).
    Daemon,
    /// Regenerate `dimux.conf` (Kitty chord mappings) if needed, then
    /// open it in `$EDITOR`/`$VISUAL`.
    Config,
}

/// `main.rs`'s actual clap entry point. `command` is `Option` so bare
/// `dimux` (no subcommand at all) parses successfully instead of
/// erroring -- `main` then defaults it to [`Cli::Attach`].
#[derive(clap::Parser)]
#[command(name = "dimux", about = "A terminal multiplexer. With no subcommand, attaches to the TUI.")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Cli>,
}
```

The explicit `about` is required, not cosmetic: the crate has no `description`
in `Cargo.toml`, so without it clap falls back to the struct's doc comment and
prints that internal note (rustdoc link syntax and all) as the first line of
`dimux --help`.

- [ ] **Step 3: Update `main.rs` to parse `Args` and default to `Cli::Attach`**

Replace `src/main.rs` in full:

```rust
use clap::Parser;
use dimux::cli::{self, Args, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Args::parse().command.unwrap_or(Cli::Attach);
    match cli {
        Cli::Attach => dimux::tui::run().await,
        Cli::Daemon => {
            let daemon = dimux::daemon::run(dimux::protocol::socket_path()).await?;
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
        Cli::Config => cli::run_config().await,
        other => cli::run(other).await,
    }
}
```

- [ ] **Step 4: Add a `run_config` stub in `cli.rs` (real implementation in Task 2) so the crate compiles**

Add near `run_server`/`run_client` in `src/cli.rs` (this will be filled in properly in Task 2 — for now just enough to compile and keep `cli::run`'s `Cli::Config` arm from being reachable through it):

```rust
pub async fn run_config() -> anyhow::Result<()> {
    unimplemented!("filled in by Task 2")
}
```

- [ ] **Step 5: Update `cli::run`'s catch-all match to also reject `Cli::Config`**

`src/cli.rs:200-210`'s `run` function is only ever called by `main.rs` with `Server`/`Client` (main.rs handles `Attach`/`Daemon`/`Config` itself directly). Update the comment and match to include `Config`:

```rust
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli {
        Cli::Server { cmd } => run_server(cmd).await,
        Cli::Client { cmd } => run_client(cmd).await,
        // `main.rs` handles `Attach`/`Daemon`/`Config` itself and never
        // calls `run` with them; a caller doing so anyway is a
        // `main.rs` bug, not something to service here.
        Cli::Attach | Cli::Daemon | Cli::Config => {
            anyhow::bail!("cli::run called with Attach/Daemon/Config, which main.rs should handle directly")
        }
    }
}
```

- [ ] **Step 6: Run the new tests**

Run: `cargo test -p dimux --lib cli:: 2>&1 | tail -20`
Expected: `bare_invocation_defaults_to_attach` and `config_subcommand_parses` PASS.

- [ ] **Step 7: Manual check — `--help` still lists every subcommand, bare invocation parses**

Run: `cargo run -- --help 2>&1 | tail -20`
Expected: `Usage: dimux [COMMAND]` (note `[COMMAND]`, not a bare `COMMAND` — confirms it's optional), and `config` listed among `attach`/`server`/`client`/`daemon`/`help`.

- [ ] **Step 8: Commit**

```bash
git add src/main.rs src/cli.rs
git commit -m "feat(cli): bare dimux defaults to attach, add config subcommand stub"
```

---

### Task 2: `kitty_setup.rs` — split out a fallible `ensure_config_written`, implement `run_config`

**Files:**
- Modify: `src/tui/kitty_setup.rs:99-153`
- Modify: `src/cli.rs` (the `run_config` stub from Task 1)
- Test: `src/tui/kitty_setup.rs` (existing test module)

- [ ] **Step 1: Write the failing test for `ensure_config_written`'s `Ok(PathBuf)` return**

Add to `src/tui/kitty_setup.rs`'s test module, right after `ensure_installed_writes_dimux_conf_and_patches_kitty_conf`:

```rust
#[test]
fn ensure_config_written_returns_the_dimux_conf_path() {
    let dir = std::env::temp_dir().join(format!("dmx-kitty-config-path-{}", std::process::id()));
    setup_fake_kitty_config(&dir, Some(""), None);
    let path = with_fake_kitty_env(&dir, || ensure_config_written().unwrap());
    assert_eq!(path, dir.join("dimux.conf"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_config_written_errors_when_not_under_kitty() {
    let prev = std::env::var_os("KITTY_WINDOW_ID");
    unsafe {
        std::env::remove_var("KITTY_WINDOW_ID");
    }
    let result = ensure_config_written();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("KITTY_WINDOW_ID", v),
            None => std::env::remove_var("KITTY_WINDOW_ID"),
        }
    }
    assert!(result.is_err(), "dimux config should surface a real error, not silently no-op, when not under Kitty");
}
```

Run: `cargo check --tests 2>&1 | head -20`
Expected: compile error — `ensure_config_written` doesn't exist yet.

- [ ] **Step 2: Split `try_ensure_installed` into a public, fallible `ensure_config_written`**

Replace `src/tui/kitty_setup.rs:99-153` (from the `ensure_installed` doc comment through the end of `try_ensure_installed`):

```rust
/// Best-effort install of dimux's Kitty chord mappings: writes
/// `<kitty-config-dir>/dimux.conf` (regenerated fresh every time, since
/// it's marked as dimux-owned -- see [`GENERATED_MARKER`]) and adds
/// `include dimux.conf` to the top of `kitty.conf` if not already
/// present. A complete no-op, silently, in any of these cases:
/// - not running under Kitty at all (see [`running_under_kitty`]) --
///   nothing to install into.
/// - `kitty.conf` doesn't exist yet -- a from-scratch Kitty install with
///   no config file at all is unusual enough, and risky enough to get
///   wrong (what should the *rest* of a from-scratch config look like?),
///   that this only ever patches an existing file, never creates one.
/// - `dimux.conf` exists but does NOT start with [`GENERATED_MARKER`] --
///   treated as a user's own hand-written file dimux must never
///   overwrite, even if it happens to share the name. Uses `.starts_with`
///   after a read rather than a separate sentinel/lock file so "did
///   dimux write this" is self-contained in the file itself, inspectable
///   by just opening it.
/// - any I/O error along the way (permissions, disk full, etc.).
///
/// Called once at the top of [`super::run`], before anything else --
/// see that call site for why a failure here must never propagate.
/// Thin wrapper over [`ensure_config_written`] that swallows every
/// error for that silent-failure contract; `dimux config` (which wants
/// a *real* error, not silence) calls `ensure_config_written` directly.
pub fn ensure_installed() {
    let _ = ensure_config_written();
}

/// The fallible core of [`ensure_installed`]: same silent-no-op rules
/// documented there (not under Kitty / no existing `kitty.conf` / a
/// hand-written `dimux.conf`), but returns those as `Ok(path)` --
/// callers that only care about "did it run" use [`ensure_installed`]
/// instead. Also returns `Ok(path)` when a hand-written `dimux.conf`
/// blocked the actual write, since `dimux config` still wants to open
/// *that* file (just not overwrite it first). Returns an `Err` only for
/// a genuine failure: not running under Kitty at all (nothing to open),
/// no resolvable Kitty config directory, or an I/O error. On success,
/// returns the path to `dimux.conf` (written or left alone) so the
/// caller can open it directly rather than recomputing the path itself.
pub fn ensure_config_written() -> anyhow::Result<PathBuf> {
    if !running_under_kitty() {
        anyhow::bail!("not running inside a Kitty window (KITTY_WINDOW_ID is unset)");
    }
    let config_dir = kitty_config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve a Kitty config directory (no $HOME or $KITTY_CONFIG_DIRECTORY)"))?;
    let dimux_conf = config_dir.join("dimux.conf");
    let kitty_conf = config_dir.join("kitty.conf");
    if !kitty_conf.is_file() {
        // No existing kitty.conf to patch -- nothing written, but the
        // path is still meaningful for a caller that wants to inspect
        // or create it themselves.
        return Ok(dimux_conf);
    }

    let is_ours = match std::fs::read_to_string(&dimux_conf) {
        Ok(existing) => existing.starts_with(GENERATED_MARKER),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e.into()),
    };
    if !is_ours {
        // A hand-written dimux.conf -- leave it untouched, but this is
        // still the file a caller should open.
        return Ok(dimux_conf);
    }
    std::fs::write(&dimux_conf, render_dimux_conf())?;

    let kitty_conf_text = std::fs::read_to_string(&kitty_conf)?;
    if !kitty_conf_text.lines().any(|line| line.trim() == "include dimux.conf") {
        let mut file = std::fs::OpenOptions::new().append(true).open(&kitty_conf)?;
        // A leading newline in case the existing file has no trailing
        // one, so this never gets glued onto the previous line.
        writeln!(file, "\ninclude dimux.conf")?;
    }
    Ok(dimux_conf)
}
```

- [ ] **Step 3: Update every existing test that called `try_ensure_installed` to call `ensure_config_written` instead**

In `src/tui/kitty_setup.rs`'s test module, replace every occurrence of `try_ensure_installed().unwrap()` with `ensure_config_written().unwrap()` (there are 6: in `ensure_installed_is_a_no_op_when_not_running_under_kitty`, `ensure_installed_does_nothing_if_kitty_conf_does_not_exist`, `ensure_installed_writes_dimux_conf_and_patches_kitty_conf`, `ensure_installed_does_not_duplicate_the_include_line_on_a_second_run` (2 occurrences), `ensure_installed_refreshes_a_previously_generated_dimux_conf`, `ensure_installed_never_overwrites_a_hand_written_dimux_conf`).

Note: `ensure_installed_is_a_no_op_when_not_running_under_kitty` currently asserts `try_ensure_installed().unwrap()` succeeds when not under Kitty (today's `Ok(())` no-op). Since `ensure_config_written` now returns `Err` in that case, update this specific test's assertion:

```rust
#[test]
fn ensure_installed_is_a_no_op_when_not_running_under_kitty() {
    let prev = std::env::var_os("KITTY_WINDOW_ID");
    unsafe {
        std::env::remove_var("KITTY_WINDOW_ID");
    }
    let result = ensure_config_written();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("KITTY_WINDOW_ID", v),
            None => std::env::remove_var("KITTY_WINDOW_ID"),
        }
    }
    assert!(result.is_err());
    // ensure_installed itself, which swallows the error, must still be
    // a true no-op: no dimux.conf/kitty.conf files should exist to
    // check here since none were set up by this test.
}
```

(Every other test in the module that calls `with_fake_kitty_env` already sets `KITTY_WINDOW_ID`, so their `ensure_config_written().unwrap()` calls succeed exactly as `try_ensure_installed().unwrap()` did before -- only this one test's assertion direction flips.)

- [ ] **Step 4: Run kitty_setup tests**

Run: `cargo test -p dimux --lib tui::kitty_setup 2>&1 | tail -30`
Expected: all PASS.

- [ ] **Step 5: Implement `run_config` in `cli.rs`**

Replace the Task-1 stub in `src/cli.rs`:

```rust
/// `dimux config`: regenerate `dimux.conf` (Kitty chord mappings) if
/// needed, then open it in `$EDITOR`/`$VISUAL`. Pure local file/process
/// work -- unlike every other `Cli` variant this touches, it never
/// connects to the daemon.
pub async fn run_config() -> anyhow::Result<()> {
    let path = crate::tui::kitty_setup::ensure_config_written()?;
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map_err(|_| anyhow::anyhow!("set $EDITOR or $VISUAL to use `dimux config`"))?;
    let status = std::process::Command::new(editor).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}
```

- [ ] **Step 6: Expose `kitty_setup` as `pub` from `tui`**

Check `src/tui/mod.rs:127` (`mod kitty_setup;`) -- `cli.rs` needs `crate::tui::kitty_setup::ensure_config_written` to be reachable, so change it to `pub(crate) mod kitty_setup;`.

- [ ] **Step 7: Run full check**

Run: `cargo check --all-targets 2>&1 | tail -20`
Expected: no errors.

- [ ] **Step 8: Commit**

```bash
git add src/tui/kitty_setup.rs src/tui/mod.rs src/cli.rs
git commit -m "feat(cli): implement dimux config -- regenerate dimux.conf then open \$EDITOR"
```

---

### Task 3: `protocol.rs` + `daemon/state.rs` + `daemon/mod.rs` — `ConsumeShellFallback`

**Files:**
- Modify: `src/protocol.rs` (add `Request::ConsumeShellFallback`, `Response::ShellFallback`)
- Modify: `src/daemon/state.rs` (add `used_shell_fallback` field + `consume_shell_fallback` method)
- Modify: `src/daemon/mod.rs` (dispatch arm)

- [ ] **Step 1: Write the failing state-level test**

Add to `src/daemon/state.rs`'s test module, near the pinned-dirs tests:

```rust
#[test]
fn consume_shell_fallback_returns_true_once_then_false() {
    let mut state = State::new();
    assert!(state.consume_shell_fallback(), "first call should grant the fallback");
    assert!(!state.consume_shell_fallback(), "second call should not");
    assert!(!state.consume_shell_fallback(), "third call should not");
}
```

Run: `cargo check --tests 2>&1 | head -20`
Expected: compile error -- `consume_shell_fallback` doesn't exist.

- [ ] **Step 2: Add the `Request`/`Response` variants to `protocol.rs`**

In `src/protocol.rs`, add to `enum Request` right after `PinnedDirsList` (line 316):

```rust
    /// Atomically check-and-consume the "spawn a default shell instead
    /// of showing the picker" fallback for a fresh empty-workspace
    /// attach. `Response::ShellFallback { available: true }` the very
    /// first time this is ever sent to a given daemon instance; `false`
    /// every time after, for the lifetime of that daemon process (never
    /// persisted -- a restarted daemon grants the fallback again). See
    /// `tui::App::bootstrap`'s call site for why this exists: a
    /// brand-new install should get one pane with zero clicks, but
    /// every attach after that shows the real picker.
    ConsumeShellFallback,
```

Add to `enum Response` right after `PinnedDirsList` (near line 462):

```rust
    /// Reply to `ConsumeShellFallback`.
    ShellFallback { available: bool },
```

- [ ] **Step 3: Add the field and method to `State`**

In `src/daemon/state.rs`, add to the `State` struct (after `pinned_dirs`, line 82):

```rust
    /// Whether the very first empty-workspace attach against this
    /// daemon instance has already consumed its "just spawn a default
    /// shell, skip the picker" fallback -- see
    /// `consume_shell_fallback`'s doc comment. Ephemeral like every
    /// other piece of `State` except `pinned_dirs`: a fresh daemon
    /// always starts with this `false`.
    used_shell_fallback: bool,
```

Add to `State::new()`'s constructor (in the `Self { ... }` literal, after `pinned_dirs: super::pinned_dirs::load(),`):

```rust
            used_shell_fallback: false,
```

Add the method (near `pinned_dirs`/`toggle_pinned_dir`, after `toggle_pinned_dir`):

```rust
    /// Atomically check-and-set the shell fallback. Returns `true` the
    /// first time this is ever called on this `State` (the caller
    /// should spawn a default shell), `false` every time after (the
    /// caller should show the picker instead).
    pub fn consume_shell_fallback(&mut self) -> bool {
        let available = !self.used_shell_fallback;
        self.used_shell_fallback = true;
        available
    }
```

- [ ] **Step 4: Run the state test**

Run: `cargo test -p dimux --lib daemon::state::tests::consume_shell_fallback 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Add the dispatch arm**

In `src/daemon/mod.rs`, add right after the `Request::PinnedDirsList` arm (near line 307):

```rust
        Request::ConsumeShellFallback => {
            let mut state = state.lock().await;
            Response::ShellFallback { available: state.consume_shell_fallback() }
        }
```

- [ ] **Step 6: Write a wire-level integration test**

Add to `src/daemon/mod.rs`'s test module, near the other simple request/response tests:

```rust
#[tokio::test]
async fn consume_shell_fallback_over_the_wire_grants_once_then_denies() {
    let guard = start_daemon().await;
    let mut conn = TestConn::connect(&guard.0).await;

    match conn.request(Request::ConsumeShellFallback).await {
        Response::ShellFallback { available } => assert!(available, "first call should grant"),
        other => panic!("expected ShellFallback, got {other:?}"),
    }
    match conn.request(Request::ConsumeShellFallback).await {
        Response::ShellFallback { available } => assert!(!available, "second call should deny"),
        other => panic!("expected ShellFallback, got {other:?}"),
    }
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p dimux --lib daemon:: 2>&1 | tail -30`
Expected: all PASS, including the two new tests.

- [ ] **Step 8: Commit**

```bash
git add src/protocol.rs src/daemon/state.rs src/daemon/mod.rs
git commit -m "feat(daemon): add ConsumeShellFallback request for first-attach shell fallback"
```

---

### Task 4: `tui/mod.rs` — wire the fallback into `bootstrap`, branch `confirm_attach_menu`

**Files:**
- Modify: `src/tui/mod.rs:401-437` (`bootstrap`)
- Modify: `src/tui/mod.rs:1073-1114` (`confirm_attach_menu`)

- [ ] **Step 1: Add a `bootstrap_empty_workspace` helper method**

Add this new `&mut self` async method on `App`, right after `bootstrap` (i.e. right after the closing `}` of `bootstrap`'s `impl` block body, before `request`):

```rust
    /// Called from [`Self::bootstrap`] exactly when the just-fetched
    /// workspace has no tree at all -- decides, via
    /// `Request::ConsumeShellFallback`, whether this is the very first
    /// such attach against this daemon (spawn a default shell directly,
    /// matching `cmd-d`'s own spawn args) or a later one (open the
    /// picker with no leaf yet to bind into, per
    /// `confirm_attach_menu`'s new no-`focused` branch).
    async fn bootstrap_empty_workspace(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let available = match self.request(write_half, reader, Request::ConsumeShellFallback).await? {
            Response::ShellFallback { available } => available,
            _ => false,
        };
        if available {
            let Response::ServerPane(server) = self
                .request(write_half, reader, Request::ServerSpawn { name: None, cmd: None, cwd: None })
                .await?
            else {
                return Ok(());
            };
            let req = Request::ClientSpawn {
                workspace: self.workspace.id.to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.id.to_string()),
            };
            if let Response::ClientPaneCreated { pane, .. } = self.request(write_half, reader, req).await? {
                self.focused = Some(pane);
            }
            return Ok(());
        }
        if let Response::PinnedDirsList(pinned) =
            self.request(write_half, reader, Request::PinnedDirsList).await?
        {
            self.pinned_dirs = pinned;
        }
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            self.attach_menu = Some(AttachMenu {
                servers: group_servers_by_cwd(servers, &self.pinned_dirs),
                selected: 0,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            });
        }
        Ok(())
    }
```

- [ ] **Step 2: Call it from `bootstrap` and from `switch_workspace`**

Replace `bootstrap`'s `Snapshot` match arm (`src/tui/mod.rs:413-427`):

```rust
                ServerMessage::Response(Response::Snapshot { workspace, grids }) => {
                    let focused = first_leaf(&workspace);
                    let is_empty = workspace.tree.is_none();
                    let mut app = App {
                        workspace,
                        grids: grids.into_iter().map(|g| (g.server_pane, g)).collect(),
                        pane_sizes: HashMap::new(),
                        focused,
                        attach_menu: None,
                        frame_area: ratatui::layout::Rect::default(),
                        dragging_split: None,
                        collapsed_groups: HashSet::new(),
                        attach_menu_preview: None,
                        pinned_dirs: Vec::new(),
                    };
                    if is_empty {
                        app.bootstrap_empty_workspace(write_half, reader).await?;
                    }
                    return Ok(app);
                }
```

Note: `switch_workspace` (a separate method, further down) also lands on a possibly-empty workspace when the user switches to a numbered workspace that doesn't exist yet (`cmd-1..9`). This plan deliberately does NOT extend the fallback/picker there -- `switch_workspace` already has its own "empty workspace -> static placeholder" behavior via `render::draw`, and the design doc's "Non-Goals" section scopes this feature to the *startup* case only. Leave `switch_workspace` untouched.

- [ ] **Step 3: Branch `confirm_attach_menu` on `self.focused`**

Replace `src/tui/mod.rs:1073-1113` in full:

```rust
    async fn confirm_attach_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(row) = self.selected_attach_menu_row() else { return Ok(()) };
        let Some(menu) = self.attach_menu.take() else { return Ok(()) };
        let adding_tab = menu.adding_tab;
        let target = match row {
            AttachMenuRow::Server(server_index) => menu.servers[server_index].1.id.to_string(),
            AttachMenuRow::SpawnNew => match self
                .request(write_half, reader, Request::ServerSpawn { name: None, cmd: None, cwd: None })
                .await?
            {
                Response::ServerPane(info) => info.id.to_string(),
                _ => return Ok(()),
            },
            AttachMenuRow::SpawnNewInGroup(server_index) => {
                let cwd = menu.servers[server_index].0.clone();
                match self
                    .request(
                        write_half,
                        reader,
                        Request::ServerSpawn { name: None, cmd: None, cwd: Some(cwd) },
                    )
                    .await?
                {
                    Response::ServerPane(info) => info.id.to_string(),
                    _ => return Ok(()),
                }
            }
            AttachMenuRow::GroupHeader(_) => return Ok(()),
        };
        match self.focused {
            Some(pane) if adding_tab => {
                let req = Request::ClientAddTab { workspace: self.workspace.id.to_string(), pane, target };
                let _ = self.request(write_half, reader, req).await?;
            }
            Some(pane) => {
                let req = Request::ClientBind { workspace: self.workspace.id.to_string(), pane, target };
                let _ = self.request(write_half, reader, req).await?;
            }
            None => {
                // No leaf exists yet -- this is the startup picker on an
                // empty workspace (see `bootstrap_empty_workspace`).
                // `adding_tab` is always `false` here (there's no leaf
                // to append a tab to), so no branch on it is needed.
                let req = Request::ClientSpawn {
                    workspace: self.workspace.id.to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(target),
                };
                if let Response::ClientPaneCreated { pane, .. } = self.request(write_half, reader, req).await? {
                    self.focused = Some(pane);
                }
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the crate check**

Run: `cargo check --all-targets 2>&1 | tail -30`
Expected: no errors.

- [ ] **Step 5: Write the bootstrap tests**

Add to `src/tui/mod.rs`'s test module, near `app_against_real_daemon`:

```rust
#[tokio::test]
async fn bootstrap_on_a_fresh_daemon_with_an_empty_workspace_spawns_a_default_shell() {
    let (app, _write_half, _reader) = app_against_real_daemon().await;
    assert!(app.attach_menu.is_none(), "the very first attach should not open the picker");
    assert!(app.focused.is_some(), "a default shell should already be spawned and focused");
    assert!(app.workspace.tree.is_some(), "the workspace should no longer be empty");
}

#[tokio::test]
async fn bootstrap_on_a_second_empty_workspace_attach_opens_the_picker_instead() {
    // Short filename -- mirrors `app_against_real_daemon`'s own naming
    // scheme (see its doc comment for why this stays short).
    let socket_path = std::env::temp_dir().join(format!("dmx-boot2-{}.sock", std::process::id()));
    crate::daemon::run(socket_path.clone()).await.expect("daemon should bind and start");

    // First attach: consumes the fallback, spawns a shell in workspace "1".
    let stream = tokio::net::UnixStream::connect(&socket_path).await.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = FrameReader::spawn(read_half);
    let _first = App::bootstrap(&mut write_half, &mut reader, "1").await.expect("bootstrap workspace 1");

    // Second attach, to a *different* (still-empty) workspace: the
    // fallback has already been consumed, so this should get the
    // picker instead, not another free shell.
    let stream2 = tokio::net::UnixStream::connect(&socket_path).await.expect("connect");
    let (read_half2, mut write_half2) = stream2.into_split();
    let mut reader2 = FrameReader::spawn(read_half2);
    let second = App::bootstrap(&mut write_half2, &mut reader2, "2").await.expect("bootstrap workspace 2");

    assert!(second.attach_menu.is_some(), "the second empty-workspace attach should open the picker");
    assert_eq!(second.focused, None, "no leaf exists yet, so nothing should be focused");
    assert!(second.workspace.tree.is_none(), "the picker path must not itself create a leaf");
}

#[tokio::test]
async fn confirm_attach_menu_with_no_focused_pane_spawns_the_first_leaf() {
    let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
    // `app_against_real_daemon` already consumed the shell fallback and
    // has one focused pane -- simulate the "picker, no leaf yet" state
    // directly rather than standing up a second daemon.
    app.focused = None;
    let Response::ServerPane(existing) = app
        .request(
            &mut write_half,
            &mut reader,
            Request::ServerSpawn { name: None, cmd: Some("cat".to_string()), cwd: None },
        )
        .await
        .unwrap()
    else {
        panic!("expected ServerPane");
    };
    let existing_id = existing.id;
    app.attach_menu = Some(AttachMenu {
        servers: vec![("Unknown".to_string(), existing)],
        selected: 0,
        pending_delete: None,
        rename: None,
        previously_bound: None,
        spawn_in_group: None,
        adding_tab: false,
    });

    app.confirm_attach_menu(&mut write_half, &mut reader).await.unwrap();

    assert!(app.focused.is_some(), "confirming on a leaf-less workspace should focus the newly created leaf");
    let Response::ClientPaneList { panes, .. } = app
        .request(&mut write_half, &mut reader, Request::ClientList { workspace: Some("1".to_string()) })
        .await
        .unwrap()
    else {
        panic!("expected ClientPaneList");
    };
    let pane = panes.iter().find(|p| Some(p.id) == app.focused).expect("focused pane should exist");
    assert_eq!(pane.active_bound(), Some(existing_id));
}
```

**This is a breaking change to an existing, heavily-relied-upon test helper.** `app_against_real_daemon` (`src/tui/mod.rs:2665-2677`) calls `App::bootstrap(&mut write_half, &mut reader, "1")` against a freshly started daemon. Before this task, that always returned an empty, unfocused workspace `"1"`. After Step 1-2's change, it now spawns a free default shell into workspace `"1"` and focuses it (the fallback always fires on a truly fresh daemon). Confirmed by inspection: **7 existing tests** call `app_against_real_daemon()` and then immediately send their own `Request::ClientSpawn { workspace: "1".to_string(), split_of: None, ... }` (`detach_and_reattach_on_a_multi_tab_leaf_preserves_the_other_tab`, `open_add_tab_menu_then_confirm_appends_rather_than_replaces`, `spawn_in_group_enter_spawns_binds_and_sends_typed_text`, `spawn_in_group_shift_enter_spawns_and_sends_but_leaves_unbound`, `spawn_in_group_row_nav_keys_move_selection_without_opening_the_field`, `refresh_attach_menu_preview_fetches_the_selected_servers_output`, `refresh_attach_menu_preview_clears_when_selection_leaves_a_server_row`, `toggle_directory_pin_on_a_non_header_row_is_a_no_op`) — `client_spawn`'s own validation (`state.rs`'s `"workspace {workspace} already has panes; name a pane to split instead"`) will now reject every one of these calls, since workspace `"1"` already has the fallback's leaf in it.

- [ ] **Step 6: Fix `app_against_real_daemon` to close the fallback's leaf before returning, restoring the "fresh, empty workspace" contract every existing test relies on**

Replace `src/tui/mod.rs:2665-2677` in full:

```rust
    /// Connect a fresh `App` (via `bootstrap`) to a real, freshly started
    /// daemon at workspace `"1"`, for the attach-menu-against-a-live-
    /// daemon tests below. Returns the `App` plus the split connection
    /// halves every `App` method needs.
    ///
    /// `bootstrap` now spawns a free default shell into a truly empty
    /// workspace (the shell fallback -- see `bootstrap_empty_workspace`),
    /// which every test in this module that predates that feature
    /// assumes does NOT exist (they immediately send their own
    /// `ClientSpawn { workspace: "1", split_of: None, .. }`, which errors
    /// against a workspace that already has a pane). Closing that one
    /// leaf here, then re-subscribing to pick up the now-empty tree and
    /// clear `focused`, restores the exact "freshly bootstrapped, empty
    /// workspace" state this helper always promised, without touching
    /// any of its 7+ existing callers.
    async fn app_against_real_daemon() -> (App, OwnedWriteHalf, FrameReader) {
        // Short filename -- `dimux-test-<full-uuid>.sock` under a long
        // macOS temp dir can exceed `SUN_LEN`; this stays well under it.
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket_path = std::env::temp_dir().join(format!("dmx-am-{}-{id}.sock", std::process::id()));
        crate::daemon::run(socket_path.clone()).await.expect("daemon should bind and start");
        let stream = tokio::net::UnixStream::connect(&socket_path).await.expect("connect to test daemon");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FrameReader::spawn(read_half);
        let mut app = App::bootstrap(&mut write_half, &mut reader, "1").await.expect("bootstrap workspace 1");
        if let Some(pane) = app.focused.take() {
            let req = Request::ClientClose { workspace: app.workspace.id.to_string(), pane };
            let _ = app.request(&mut write_half, &mut reader, req).await.expect("close the shell-fallback leaf");
        }
        (app, write_half, reader)
    }
```

- [ ] **Step 7: Run every test that calls `app_against_real_daemon` and confirm they all still pass unmodified**

Run: `cargo test -p dimux --lib tui:: 2>&1 | tail -60`
Expected: ALL PASS, including the 8 tests named above plus the 3 new bootstrap tests from Step 5. If any of the 8 pre-existing tests still fails, its failure message will name the assertion that broke; re-read that test's setup in `src/tui/mod.rs` to see what it still assumes about workspace `"1"`'s starting state, and fix `app_against_real_daemon` further (not the individual test) so the invariant holds again.

- [ ] **Step 8: Run the full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: ALL PASS. If Step 7 surfaced any test needing adjustment, fix it here and re-run.

- [ ] **Step 9: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat(tui): open the picker (or a default shell) on an empty-workspace attach"
```

---

### Task 5: Final verification and PR

**Files:** None new — verification only.

- [ ] **Step 1: Full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: ALL PASS.

- [ ] **Step 2: Clippy**

Run: `cargo clippy -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 3: Release build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: success.

- [ ] **Step 4: Manual smoke test**

```bash
cargo build --release
./target/release/dimux --help  # confirm `config` is listed, [COMMAND] is optional
EDITOR=cat ./target/release/dimux config  # should print the generated dimux.conf if running under Kitty, or a clear "not running inside Kitty" error otherwise
```

- [ ] **Step 5: Push and open draft PR**

```bash
git push -u origin feat/default-attach-startup-picker
gh pr create --draft --title "feat: default to attach, startup picker, dimux config" --body "$(cat <<'EOF'
## Summary
- Bare `dimux` (no subcommand) now launches the TUI, same as `dimux attach`.
- The very first empty-workspace attach against a fresh daemon spawns a default shell directly; every attach after that shows the server-pane picker instead of the old static placeholder.
- New `dimux config` subcommand: regenerates `dimux.conf` (Kitty chord mappings) if needed, then opens it in `$EDITOR`/`$VISUAL`.

See `docs/superpowers/specs/2026-08-06-default-attach-and-startup-picker-design.md` and the matching plan doc for full design/rationale.

## Test plan
- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` clean
- [ ] Manual: `dimux` with no args opens the TUI
- [ ] Manual: fresh daemon + `dimux attach` on an empty workspace gets a shell with zero keypresses; a second attach to another empty workspace shows the picker
- [ ] Manual: `dimux config` opens dimux.conf in `$EDITOR`
EOF
)"
```

- [ ] **Step 6: Report the PR URL**
