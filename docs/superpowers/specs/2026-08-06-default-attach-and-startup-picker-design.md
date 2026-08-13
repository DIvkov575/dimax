# Default Attach, Startup Picker, and `dimax config` — Design

## Goal

Three small, related changes to how `dimax` starts up:

1. Bare `dimax` (no subcommand) launches the TUI, same as `dimax attach`.
2. Attaching to an empty workspace shows the server-pane picker instead of a static placeholder — except for the very first attach against a fresh daemon, which just spawns a default shell directly (so a brand-new install still gets a pane with zero clicks).
3. `dimax config` opens the generated `dimax.conf` (Kitty chord mappings) in `$EDITOR`, regenerating it first if needed.

## CLI Entry Point

`Cli` (in `cli.rs`) gains a `Config` variant:

```rust
pub enum Cli {
    Attach,
    Server { cmd: ServerCmd },
    Client { cmd: ClientCmd },
    Daemon,
    /// Regenerate `dimax.conf` (Kitty chord mappings) if needed, then
    /// open it in `$EDITOR`/`$VISUAL`.
    Config,
}
```

`main.rs`'s top-level arg struct becomes:

```rust
#[derive(clap::Parser)]
#[command(name = "dimax")]
struct Args {
    #[command(subcommand)]
    command: Option<Cli>,
}
```

with `main` doing `Args::parse().command.unwrap_or(Cli::Attach)` before the existing `match`. No change to any existing subcommand's parsing; `dimax`, `dimax attach`, and `dimax server ls` etc. all keep working exactly as today, and `dimax --help` still lists every subcommand (clap's `Option<Subcommand>` prints the same help either way).

`Cli::Config`'s handler (in `cli.rs`, alongside `run_server`/`run_client`) does not touch the daemon at all — it's pure local file/process work:

```rust
async fn run_config() -> anyhow::Result<()> {
    let path = crate::tui::kitty_setup::ensure_config_written()?;
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map_err(|_| anyhow::anyhow!("set $EDITOR or $VISUAL to use `dimax config`"))?;
    let status = std::process::Command::new(editor).arg(path).status()?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}
```

`kitty_setup::ensure_installed` (today's best-effort, silent-on-failure, `dimax attach`-triggered installer) is refactored to expose a `pub fn ensure_config_written() -> anyhow::Result<PathBuf>` — the actual write logic — with `ensure_installed` becoming a thin `let _ = ensure_config_written();` wrapper preserving its existing silent-failure contract for the attach path. `dimax config` uses the fallible version directly so a real error (not running under Kitty, unwritable config dir) surfaces to the user instead of silently doing nothing before opening a possibly-stale or nonexistent file.

## Startup Picker

### New daemon state

`State` gains one new field, ephemeral like everything except `pinned_dirs`:

```rust
/// Whether the very first empty-workspace attach against this daemon
/// instance has already consumed its "just spawn a default shell, skip
/// the picker" fallback. `false` until `consume_shell_fallback` is
/// first called; never persisted -- a fresh daemon always gets a fresh
/// first-attach experience, matching every other piece of State except
/// `pinned_dirs`.
used_shell_fallback: bool,
```

```rust
/// Atomically check-and-set the shell fallback. Returns `true` the
/// first time this is ever called on this `State` (the caller should
/// spawn a default shell), `false` every time after (the caller should
/// show the picker instead).
pub fn consume_shell_fallback(&mut self) -> bool {
    let available = !self.used_shell_fallback;
    self.used_shell_fallback = true;
    available
}
```

### New request/response

```rust
// Request
ConsumeShellFallback,
```
```rust
// Response
ShellFallback { available: bool },
```

Dispatch arm: `Response::ShellFallback { available: state.consume_shell_fallback() }`. No workspace/pane arguments — this is a pure daemon-instance-lifetime flag, unrelated to which workspace is being attached to.

### `App::bootstrap` changes

Today, `bootstrap` sends `Subscribe`, gets back a `Snapshot`, and returns the `App` unconditionally — an empty workspace just renders `render::draw`'s "(empty workspace — press cmd-d to spawn a pane)" placeholder and waits for input.

New sequence, only when the returned `Snapshot`'s `workspace.tree` is `None`:

1. Send `Request::ConsumeShellFallback`.
2. If `available: true`: send `Request::ServerSpawn { name: None, cmd: None, cwd: None }` (identical args to `cmd-d`'s existing spawn), then `Request::ClientSpawn { workspace: id, split_of: None, dir: None, bind: Some(new_server_pane_id) }`. Re-fetch via `Subscribe` (or apply the resulting `LayoutDelta`/use the `ClientPaneCreated` response directly) so the returned `App` has a real, non-empty `workspace.tree` and `focused` pointing at the new leaf.
3. If `available: false`: build the same picker `detach_and_open_menu`/`open_add_tab_menu` construct (fetch `PinnedDirsList` + `ServerList`, `group_servers_by_cwd`), and return the `App` with `attach_menu: Some(AttachMenu { ..., previously_bound: None, adding_tab: false })` set. `focused` stays `None` (there is no leaf).

Both branches are extracted into a helper (e.g. `bootstrap_empty_workspace`) called from `bootstrap` right before constructing the final `App`, keeping `bootstrap` itself readable.

### Picker confirm on a leaf-less workspace

`confirm_attach_menu` currently does:
```rust
let Some(pane) = self.focused else { return Ok(()) };
...
let req = Request::ClientBind { workspace: ..., pane, target };
```

This silently no-ops when `focused` is `None` — exactly the state the picker now opens in. Fixed by branching:

```rust
let target = /* existing match on row, unchanged */;
let req = match self.focused {
    Some(pane) if menu.adding_tab => Request::ClientAddTab { workspace: ..., pane, target },
    Some(pane) => Request::ClientBind { workspace: ..., pane, target },
    None => Request::ClientSpawn {
        workspace: self.workspace.id.to_string(),
        split_of: None,
        dir: None,
        bind: Some(target),
    },
};
```

On success, set `self.focused` explicitly from the `ClientPaneCreated` response's `pane` id — matching the existing pattern `App::split` already uses after its own `ClientSpawn` call, rather than relying on the pushed `LayoutDelta`/`reconcile_focus` path to set it indirectly. (`reconcile_focus` would in fact reach the same result on its own — `is_some_and` on a `None` `focused` is `false`, so it falls through to `first_leaf`, once the `LayoutDelta` this `ClientSpawn` broadcasts is applied — but setting it directly from the response is simpler and matches the one other place in this file that creates a leaf from no prior selection.)

The `GroupHeader` row (toggle collapse, not a pick) and `SpawnNewInGroup`/`SpawnNew` rows (spawn-then-bind, already going through `ServerSpawn` first) are unaffected by this branch — only the final bind-vs-spawn step changes.

## Non-Goals

- No change to `cmd-shift-z`/`cmd-t`'s existing behavior on a workspace that already has a leaf — this only affects the *startup* case (bootstrap) and, by extension, whatever `confirm_attach_menu` does when `focused` happens to be `None` (which today could only happen transiently; after this change it's a real, reachable state).
- No persistence of `used_shell_fallback` across daemon restarts — a fresh daemon (e.g. after `pkill -f "dimax daemon"` + reattach) always gets one more free shell-fallback attach, by design.
- No config-menu TUI screen — `dimax config` is a plain CLI subcommand that shells out to `$EDITOR`, not a new in-TUI menu.
- No new keybind. The picker's browse/confirm/cancel/rename/delete/pin grammar is entirely reused as-is; the only new code path is *how it's constructed* (from `bootstrap` instead of from a chord handler) and *how a confirm without `self.focused` resolves* (spawn instead of bind).

## Testing

- `daemon::state`: `consume_shell_fallback_returns_true_once_then_false` (calls it 3x, asserts `true, false, false`).
- `daemon::mod`: wire-level test for `Request::ConsumeShellFallback` returning `available: true` once then `false` on repeat calls against the same daemon.
- `cli`: no unit test for `run_config`'s `$EDITOR` exec (out of scope for automated testing — shelling to an interactive editor isn't unit-testable); do add a test that `Args::parse` with zero args yields `Cli::Attach` and that `dimax config` parses to `Cli::Config`.
- `tui::kitty_setup`: existing tests for `ensure_installed`'s idempotency/marker rules continue to pass unmodified against the renamed/split `ensure_config_written`; add one test confirming `ensure_config_written`'s `Err` path (e.g. not running under Kitty) surfaces an actual error rather than silently returning `Ok` with a stale path.
- `tui::mod`: 
  - `bootstrap_on_a_fresh_daemon_with_an_empty_workspace_spawns_a_default_shell` — first attach against a fresh test daemon lands with a non-empty tree and one focused leaf bound to a freshly spawned server-pane.
  - `bootstrap_on_a_second_empty_workspace_attach_opens_the_picker_instead` — after one attach has already consumed the fallback (simulated by calling `ConsumeShellFallback` once directly, or by bootstrapping once already), a second `bootstrap` against a *different* empty workspace returns an `App` with `attach_menu: Some(..)` and `focused: None`.
  - `confirm_attach_menu_with_no_focused_pane_spawns_the_first_leaf` — construct an `App` with `focused: None` and an `attach_menu` open on a real daemon with one existing server-pane; confirm creates a leaf bound to it and sets `focused` to the new pane's id.
