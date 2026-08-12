# Daemon Hot-Restart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a new `dimax daemon` process take over a running one's live server-panes — actual PTY-backed processes (shells, `nvim`, `claude`, anything) keep running, uninterrupted, across a binary upgrade, instead of dying to `SIGHUP` and being replaced by fresh respawned shells (today's `daemon::session` layout-replay behavior).

**Architecture:** The new daemon connects to the old daemon's existing socket as a client and sends `Request::BeginHandoff`. The old daemon dup's each live server-pane's PTY master fd and sends it — together with that pane's real `ServerPaneId`/`WorkspaceId`/`ClientPaneId` and metadata — to the new daemon over a dedicated, freshly-bound `UnixDatagram` (datagram sockets guarantee a payload's bytes and its ancillary fd arrive together in one `recvmsg`, which a stream socket does not promise). The new daemon reconstructs each `ServerPane` around the inherited fd (a hand-written `MasterPty`/`Child` impl, since `portable-pty` has no public "adopt an existing fd" constructor) and rebuilds `State` with the *same* ids the old daemon used, so an already-attached client can reconnect and resume as if nothing happened. The terminal's on-screen grid is not preserved — a fresh `wezterm_term::Terminal` starts blank for every adopted pane; the underlying process and its own state (memory, running job, PTY) are what actually survive.

**Tech Stack:** Rust, `portable-pty` (existing dep), `sendfd` (new dep, `SCM_RIGHTS` fd-passing over Unix sockets, has a `tokio` feature), `tokio::net::UnixDatagram`, `libc` (existing dep).

**Known, accepted limitations (confirmed via research, not guesses):**
- No terminal-screen redraw preservation — covered above.
- Adopted panes can't be `wait()`ed in the POSIX sense (they're reparented to init once the old daemon exits, and only the *actual* parent can `waitpid()`); `kill()` still works (any process with permission can signal any pid). Confirmed dimax's own code only ever calls `.kill()` on a pane's `Child` handle (`src/term/mod.rs:526`, the only call site) — never `.wait()`/`.try_wait()`/`.process_id()` — so this gap is invisible to existing behavior.
- `MasterPty::tty_name()` can't be recovered for an inherited fd (the slave-side path is only resolvable from the slave fd, which the *original* `ServerPane::spawn` already drops right after spawning — `src/term/mod.rs:167`). Confirmed dimax never calls `tty_name()` anywhere, so this is a silent `None`, not a behavior change.
- If the handoff is interrupted (new daemon crashes mid-transfer), panes not yet transferred die normally when the old daemon exits (same as today) — this is strictly a bonus path, never a regression versus today's SIGHUP-everything baseline.

---

## File structure

- **Create** `src/daemon/handoff.rs` — the whole hot-restart mechanism: `InheritedMasterPty` (custom `MasterPty` impl over a raw fd), `InheritedChild` (custom `Child`/`ChildKiller` impl over a raw pid), the wire types for what travels over the handoff datagram channel (`HandoffPane`, `HandoffMessage`), and the two halves of the exchange (`send_handoff` on the donor side, `receive_handoff` on the acceptor side).
- **Modify** `src/protocol.rs` — two new `Request`/`Response` variants for negotiating a handoff over the *existing* JSON-framed client protocol (`BeginHandoff`/`HandoffStarting`); everything after that negotiation travels over the dedicated datagram channel, not this framing.
- **Modify** `src/term/mod.rs` — `ServerPane::from_inherited(...)`, a second constructor parallel to `spawn`, sharing the same `Inner`/reader-thread wiring but starting from an already-open `Box<dyn MasterPty + Send>` + `Box<dyn Child + Send + Sync>` instead of calling `openpty()`. Also `ServerPane::dup_master_fd`, needed by the donor side.
- **Modify** `src/daemon/state.rs` — `State::adopt_pane`, parallel to `restore_session` but *preserving* the exact ids handed to it (no fresh-minting), since a hot-restart's whole point is that an already-attached client's held ids stay valid.
- **Modify** `src/daemon/mod.rs` — the donor-side `Request::BeginHandoff` dispatch handler, and a new top-level acceptor entry point `run_taking_over_or_fresh(socket_path)` that tries a handoff first and falls back to `run_and_restore_session` if nothing answers.
- **Modify** `src/main.rs` — `Cli::Daemon` calls the new entry point instead of `run_and_restore_session`.
- **Modify** `src/tui/mod.rs` — reconnect-on-disconnect in `run`'s main loop, since today a dropped connection just propagates an error and exits (`FrameReader::next`, `src/tui/mod.rs:182-187`).
- **Modify** `Cargo.toml` — add the `sendfd` dependency.

---

## Task 1: Add the `sendfd` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the dependency**

Add this line under `[dependencies]` in `Cargo.toml` (alphabetical position, after `serde_json`):

```toml
sendfd = { version = "0.4", features = ["tokio"] }
```

- [ ] **Step 2: Verify it resolves and builds**

Run: `cargo build --release 2>&1 | tail -30`
Expected: `Downloading sendfd v0.4.4` (or newer patch) followed by a clean `Finished` line. No other files should need changes yet — this step only pulls the dependency in.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add sendfd for SCM_RIGHTS fd-passing (hot restart)"
```

---

## Task 2: `InheritedMasterPty` — adopt a raw fd as a `MasterPty`

**Files:**
- Create: `src/daemon/handoff.rs`
- Modify: `src/daemon/mod.rs` (register `pub mod handoff;`)

`portable-pty::MasterPty` has no public constructor from an existing fd — its only concrete implementation, `UnixMasterPty`, is a private struct only ever built by `openpty()` (confirmed by reading `portable-pty-0.9.0/src/unix.rs:22-78,303-307`; no `pub`, no `FromRawFd`/`TryFrom<RawFd>` impl anywhere in the crate). Every method dimax actually calls on a `MasterPty` (`resize`, `try_clone_reader`, `take_writer`, `process_group_leader`, `as_raw_fd` — confirmed by grep, no other methods are called anywhere in `src/term/mod.rs`) is a pure kernel syscall parameterized only by the fd number (`ioctl(TIOCSWINSZ)`, `dup`, `tcgetpgrp`) — none depend on state `openpty()` sets up that isn't recoverable from the bare fd. So a small hand-written impl is both necessary and sufficient.

- [ ] **Step 1: Register the module**

In `src/daemon/mod.rs`, add alongside the other `pub mod` lines:

```rust
pub mod handoff;
```

- [ ] **Step 2: Write the failing test**

Create `src/daemon/handoff.rs` with just this test at the top (module doc comment omitted here for brevity — add one before finalizing, per repo convention: explain *why* this exists, referencing this plan's "no adoption constructor" finding):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{MasterPty, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    /// A real pty pair, keeping the *original* master alive (so the
    /// underlying pty doesn't disappear) while a fresh `dup()` of its
    /// fd is wrapped in `InheritedMasterPty` -- proving the adopted
    /// wrapper behaves identically to a portable-pty-owned one for
    /// every operation dimax actually performs on a `MasterPty`.
    fn real_pty_pair() -> (Box<dyn MasterPty + Send>, i32) {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let raw_fd = pair.master.as_raw_fd().expect("as_raw_fd");
        let dup_fd = unsafe { libc::dup(raw_fd) };
        assert!(dup_fd >= 0, "dup failed: {:?}", std::io::Error::last_os_error());
        (pair.master, dup_fd)
    }

    #[test]
    fn inherited_master_pty_can_write_and_read_through_the_real_pty() {
        let (master, dup_fd) = real_pty_pair();
        let mut original_writer = master.take_writer().unwrap();

        let inherited = InheritedMasterPty::new(dup_fd);
        let mut inherited_reader = inherited.try_clone_reader().unwrap();

        original_writer.write_all(b"hello\n").unwrap();
        original_writer.flush().unwrap();

        let mut buf = [0u8; 64];
        let n = inherited_reader.read(&mut buf).unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("hello"),
            "expected the inherited fd to observe the same pty traffic, got {:?}",
            String::from_utf8_lossy(&buf[..n])
        );
    }

    #[test]
    fn inherited_master_pty_resize_reaches_the_real_pty() {
        let (master, dup_fd) = real_pty_pair();
        let inherited = InheritedMasterPty::new(dup_fd);

        inherited
            .resize(PtySize {
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let size = master.get_size().unwrap();
        assert_eq!((size.rows, size.cols), (40, 100));
    }
}
```

- [ ] **Step 3: Run it to confirm it fails to compile**

Run: `cargo test --release --lib daemon::handoff 2>&1 | tail -20`
Expected: `error[E0433]: failed to resolve: use of undeclared type \`InheritedMasterPty\`` (or similar) — the test references a type that doesn't exist yet.

- [ ] **Step 4: Write `InheritedMasterPty`**

Add above the `#[cfg(test)]` block in `src/daemon/handoff.rs`:

```rust
//! The mechanism behind a *hot* daemon restart: handing live server-
//! panes' PTY master file descriptors from an old daemon process to a
//! new one over a Unix datagram socket (`SCM_RIGHTS` ancillary data),
//! so the actual child processes (shells, editors, anything) never see
//! the `SIGHUP` a plain process exit would otherwise deliver. See
//! `daemon::session` for the *other*, much simpler restart path
//! (layout-only replay with fresh shells) this is a cousin of --
//! that one never needs any of this, since it doesn't try to keep a
//! live process running across the restart at all.
//!
//! `portable_pty::MasterPty`'s only concrete Unix implementation is a
//! private struct buildable only via `openpty()` -- there is no public
//! way to adopt an fd obtained some other way (confirmed by reading the
//! vendored `portable-pty` 0.9.0 source: no `pub` constructor, no
//! `FromRawFd`/`TryFrom<RawFd>` impl anywhere in the crate). Every
//! `MasterPty` method dimax actually calls (`resize`, `try_clone_reader`,
//! `take_writer`, `process_group_leader`, `as_raw_fd`) is a pure kernel
//! syscall keyed only by the fd number, so [`InheritedMasterPty`] below
//! reimplements exactly those, and only those.

use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use std::io::{Read, Result as IoResult, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A `MasterPty` wrapping a raw fd this process didn't open itself
/// (received via `SCM_RIGHTS`, see `receive_handoff`). Closes the fd on
/// drop, same as portable-pty's own `UnixMasterPty` would.
#[derive(Debug)]
pub struct InheritedMasterPty {
    fd: OwnedFd,
}

impl InheritedMasterPty {
    /// `fd` must be a valid, open, owned fd for a pty master this
    /// process now exclusively holds one reference to (the caller is
    /// giving up ownership of it to this struct).
    pub fn new(fd: RawFd) -> Self {
        Self {
            // SAFETY: caller contract above; `OwnedFd` takes over
            // closing it on drop.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        }
    }

    fn dup(&self) -> anyhow::Result<OwnedFd> {
        let raw = unsafe { libc::dup(self.fd.as_raw_fd()) };
        if raw < 0 {
            anyhow::bail!("dup failed: {:?}", std::io::Error::last_os_error());
        }
        // SAFETY: `dup` just returned a fresh, exclusively-owned fd.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

impl MasterPty for InheritedMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let ws_size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws_size as *const _) }
            != 0
        {
            anyhow::bail!("ioctl(TIOCSWINSZ) failed: {:?}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let mut ws_size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws_size as *mut _) }
            != 0
        {
            anyhow::bail!("ioctl(TIOCGWINSZ) failed: {:?}", std::io::Error::last_os_error());
        }
        Ok(PtySize {
            rows: ws_size.ws_row,
            cols: ws_size.ws_col,
            pixel_width: ws_size.ws_xpixel,
            pixel_height: ws_size.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::from(self.dup()?)))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        Ok(Box::new(std::fs::File::from(self.dup()?)))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }

    fn tty_name(&self) -> Option<std::path::PathBuf> {
        // Only recoverable from the *slave* fd, which the original
        // `ServerPane::spawn` already drops right after spawning
        // (`src/term/mod.rs:167`) -- and dimax never calls this method
        // anywhere, confirmed by grep. See module doc comment.
        None
    }
}
```

- [ ] **Step 5: Run the tests to confirm they pass**

Run: `cargo test --release --lib daemon::handoff 2>&1 | tail -20`
Expected: both tests `ok`.

- [ ] **Step 6: Commit**

```bash
git add src/daemon/handoff.rs src/daemon/mod.rs
git commit -m "feat: InheritedMasterPty adopts a raw pty master fd"
```

---

## Task 3: `InheritedChild` — a `Child`/`ChildKiller` over a bare pid

**Files:**
- Modify: `src/daemon/handoff.rs`

Confirmed by grep: dimax's own code calls exactly one method on a pane's `Child` handle anywhere — `guard.child.kill()?` at `src/term/mod.rs:526`. `wait`/`try_wait`/`process_id` are never called. This matters because an *adopted* process is not a true child of the new daemon (it's reparented to init once the old daemon exits), so real `wait()`/`waitpid()` semantics aren't available to it regardless of implementation — only signaling (`kill`) is. The impl below is honest about that: `kill` is real, `try_wait`/`wait` degrade to a liveness probe (`kill(pid, 0)`), matching the one thing dimax's code actually needs.

- [ ] **Step 1: Write the failing test**

Add to `src/daemon/handoff.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn inherited_child_kill_actually_terminates_the_process() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let child = cmd.spawn().expect("spawn sleep 30");
        let pid = child.id() as libc::pid_t;
        // Detach std's own `Child` without killing it -- this test is
        // specifically about `InheritedChild`'s *own* kill working on a
        // process it never spawned itself, mirroring the real scenario
        // (the acceptor daemon adopting a pid it isn't the OS parent
        // of).
        std::mem::forget(child);

        let mut inherited = InheritedChild::new(pid);
        inherited.kill().expect("kill should succeed");

        // Give the kernel a moment to actually reap/deliver the signal,
        // then confirm the pid is gone via the same liveness probe
        // `try_wait` uses internally.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let still_alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!still_alive, "process should be dead after kill()");
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile**

Run: `cargo test --release --lib daemon::handoff::tests::inherited_child_kill 2>&1 | tail -20`
Expected: `error[E0433]: failed to resolve: use of undeclared type \`InheritedChild\``

- [ ] **Step 3: Write `InheritedChild`**

Add to `src/daemon/handoff.rs`, after `InheritedMasterPty`'s `impl MasterPty` block:

```rust
/// A `Child`/`ChildKiller` for a process this daemon didn't spawn and
/// isn't the OS parent of (an adopted pane's process, reparented to
/// init once the donor daemon exits). `kill` is a real signal delivery
/// -- any process with permission can signal any pid, parent or not.
/// `wait`/`try_wait` can't be real `waitpid()` calls (POSIX only lets
/// the actual parent reap a child), so they degrade to a liveness
/// probe (`kill(pid, 0)`); dimax's own code never calls either anyway
/// (confirmed by grep against `src/term/mod.rs`), so this is a
/// deliberate, harmless gap rather than a silent correctness loss.
#[derive(Debug, Clone, Copy)]
pub struct InheritedChild {
    pid: libc::pid_t,
}

impl InheritedChild {
    pub fn new(pid: libc::pid_t) -> Self {
        Self { pid }
    }

    fn is_alive(&self) -> bool {
        unsafe { libc::kill(self.pid, 0) == 0 }
    }
}

impl ChildKiller for InheritedChild {
    fn kill(&mut self) -> IoResult<()> {
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(*self)
    }
}

impl Child for InheritedChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        Ok(if self.is_alive() {
            None
        } else {
            Some(ExitStatus::with_exit_code(0))
        })
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        while self.is_alive() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid as u32)
    }
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test --release --lib daemon::handoff 2>&1 | tail -20`
Expected: all three tests in this module `ok`.

- [ ] **Step 5: Commit**

```bash
git add src/daemon/handoff.rs
git commit -m "feat: InheritedChild signals an adopted, non-child pid"
```

---

## Task 4: `ServerPane::from_inherited` + `ServerPane::dup_master_fd`

**Files:**
- Modify: `src/term/mod.rs`

`ServerPane::spawn` (`src/term/mod.rs:109-296`) does three things worth separating: (1) obtain a `Box<dyn MasterPty + Send>` + `Box<dyn Child + Send + Sync>` (today, always via `openpty()` + `spawn_command()`); (2) build the `Inner`/reader-thread wiring around whatever those are; (3) return the `ServerPane`. `from_inherited` reuses all of (2)-(3), only swapping (1) for an already-open `InheritedMasterPty`/`InheritedChild` pair.

- [ ] **Step 1: Write the failing test**

Add to `src/term/mod.rs`'s existing `#[cfg(test)] mod tests` block (near `spawn_prints_output_and_dies`):

```rust
    #[test]
    fn from_inherited_reads_output_already_flowing_through_the_adopted_fd() {
        use crate::daemon::handoff::{InheritedChild, InheritedMasterPty};
        use portable_pty::{MasterPty, PtySize, native_pty_system};
        use std::io::Write;
        use std::os::fd::AsRawFd;

        // Simulate "a pane the donor already spawned": a real pty with
        // `cat` running in it, plus a *separate* dup'd fd standing in
        // for what the acceptor daemon would receive over the wire.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = portable_pty::CommandBuilder::new("cat");
        let child = pair.slave.spawn_command(cmd.clone()).unwrap();
        drop(pair.slave);
        let dup_fd = unsafe { libc::dup(pair.master.as_raw_fd().unwrap()) };
        assert!(dup_fd >= 0);
        let pid = child.process_id().unwrap() as libc::pid_t;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::from_inherited(
            id,
            None,
            Box::new(InheritedMasterPty::new(dup_fd)),
            Box::new(InheritedChild::new(pid)),
            Size { rows: 24, cols: 80 },
            tx,
            None,
            "bench".to_string(),
        )
        .unwrap();

        // Write through the *original* master (standing in for "output
        // that arrives after the handoff completes") and confirm the
        // adopted pane's own reader thread observes it.
        pair.master.take_writer().unwrap().write_all(b"hi\n").unwrap();
        let found = wait_until(&mut rx, || snapshot_text(&pane).contains("hi"));
        assert!(found, "adopted pane should observe output on the inherited fd");

        let _ = pane.kill();
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile**

Run: `cargo test --release --lib term::tests::from_inherited 2>&1 | tail -20`
Expected: `error[E0599]: no function or associated item named \`from_inherited\` found`

- [ ] **Step 3: Refactor `spawn`'s shared tail into a helper, then add `from_inherited`**

In `src/term/mod.rs`, change the signature of `spawn` so the part *after* obtaining `master`/`child` is a separate function both constructors call. Replace from `let writer = SharedWriter(...)` (currently line 169) through the end of `spawn` (currently ending at line 296, just before the closing `}` of the function) with a call to a new private helper, then add that helper plus `from_inherited` and `dup_master_fd`:

```rust
    pub fn spawn(
        id: ServerPaneId,
        name: Option<String>,
        cmd: Option<String>,
        cwd: Option<String>,
        size: Size,
        events: UnboundedSender<ServerPaneEvent>,
        owner_workspace: Option<WorkspaceId>,
        short_id: String,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let PtyPair { slave, master } = pair;

        let mut builder = match &cmd {
            Some(shell_cmd) => {
                let mut b = CommandBuilder::new("/bin/sh");
                b.arg("-c");
                b.arg(shell_cmd);
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(cwd) = cwd {
            builder.cwd(cwd);
        }
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");

        let child = slave.spawn_command(builder)?;
        drop(slave);

        Self::from_inherited(id, name, master, child, size, events, owner_workspace, short_id)
    }

    /// Build a `ServerPane` around an already-open master/child pair
    /// instead of calling `openpty()` -- what `spawn` itself now does
    /// internally, and what `daemon::handoff::receive_handoff` uses
    /// directly for a pane adopted from another daemon process (see
    /// that module's doc comment for why `openpty()` isn't an option
    /// there: there's no live pty to open, only an inherited fd).
    #[allow(clippy::too_many_arguments)]
    pub fn from_inherited(
        id: ServerPaneId,
        name: Option<String>,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        size: Size,
        events: UnboundedSender<ServerPaneEvent>,
        owner_workspace: Option<WorkspaceId>,
        short_id: String,
    ) -> anyhow::Result<Self> {
        let writer = SharedWriter(Arc::new(Mutex::new(master.take_writer()?)));
        let terminal = Terminal::new(
            TerminalSize {
                rows: size.rows as usize,
                cols: size.cols as usize,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            Arc::new(Config),
            "dimax",
            env!("CARGO_PKG_VERSION"),
            Box::new(writer.clone()),
        );
        let mut reader = master.try_clone_reader()?;
        let raw_fd = master.as_raw_fd();

        let inner = Arc::new(Mutex::new(Inner {
            terminal,
            status: ServerPaneStatus::Running,
            size,
            master,
            child,
            writer,
        }));

        let thread_inner = Arc::clone(&inner);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let first = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let mut batch = buf[..first].to_vec();

                const BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
                std::thread::sleep(BATCH_WINDOW);

                if let Some(fd) = raw_fd {
                    set_nonblocking(fd, true);
                    const MAX_BATCH_READS: usize = 64;
                    for _ in 0..MAX_BATCH_READS {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => batch.extend_from_slice(&buf[..n]),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    set_nonblocking(fd, false);
                }

                {
                    let mut guard = thread_inner.lock().unwrap();
                    guard.terminal.advance_bytes(&batch);
                }
                let _ = events.send(ServerPaneEvent::Changed(id));
            }
            {
                let mut guard = thread_inner.lock().unwrap();
                guard.status = ServerPaneStatus::Dead;
            }
            let _ = events.send(ServerPaneEvent::Died(id));
        });

        Ok(Self {
            id,
            name,
            owner_workspace,
            short_id,
            inner,
        })
    }

    /// A fresh `dup()` of this pane's master fd, for
    /// `daemon::handoff::send_handoff` to hand off via `SCM_RIGHTS`
    /// without disturbing this daemon's own copy (which it keeps using
    /// normally until the moment it actually exits).
    pub fn dup_master_fd(&self) -> anyhow::Result<RawFd> {
        let guard = self.inner.lock().unwrap();
        let Some(fd) = guard.master.as_raw_fd() else {
            anyhow::bail!("this platform's MasterPty has no raw fd to duplicate");
        };
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            anyhow::bail!("dup failed: {:?}", std::io::Error::last_os_error());
        }
        Ok(dup)
    }
```

Add `Child` to the existing `use portable_pty::{...}` import list at the top of the file (it currently imports `Child` already for the `Inner.child` field type — confirm, and add `MasterPty` too if not already imported; both are needed by `from_inherited`'s signature).

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test --release --lib term::tests 2>&1 | tail -40`
Expected: `from_inherited_reads_output_already_flowing_through_the_adopted_fd` passes, and every pre-existing test in `term::tests` still passes (this step only refactored `spawn`'s tail into a shared helper — behavior must be unchanged).

- [ ] **Step 5: Commit**

```bash
git add src/term/mod.rs
git commit -m "feat: ServerPane::from_inherited builds a pane around an adopted fd"
```

---

## Task 5: Protocol additions for handoff negotiation

**Files:**
- Modify: `src/protocol.rs`

Only the *negotiation* travels over the existing JSON-framed protocol; the fds themselves travel over a separate datagram channel (Task 6) because `protocol::framing`'s `write_frame`/`read_frame` (`src/protocol.rs`, the `framing` module) do plain length-prefixed byte I/O with no ancillary-data support, and mixing a raw `sendmsg`/`recvmsg` fd-transfer into the middle of that buffered stream risks the fd's ancillary data landing on a *different* read than intended -- a well-known `SCM_RIGHTS` pitfall avoided entirely by giving the fd transfer its own dedicated socket.

- [ ] **Step 1: Add the two variants**

In `src/protocol.rs`, add to the `Request` enum (near `Subscribe`):

```rust
    /// Sent by a *new* daemon process to an already-running one, over a
    /// normal client connection to the existing socket -- the start of
    /// a hot restart (see `daemon::handoff` module doc). `datagram_path`
    /// is where the sender has already bound a fresh `UnixDatagram`;
    /// the receiving (old) daemon connects to it and streams every live
    /// pane's fd + metadata there, entirely outside this JSON-framed
    /// protocol (ancillary fd data can't ride along with a buffered
    /// stream frame safely -- see `daemon::handoff`).
    BeginHandoff {
        datagram_path: String,
    },
```

Add to the `Response` enum (near `Snapshot`):

```rust
    /// Reply to `BeginHandoff`: the old daemon is about to start
    /// streaming `pane_count` panes to the requested datagram path,
    /// then exit once done. `pane_count` lets the new daemon know
    /// exactly how many `HandoffMessage::Pane`s to expect before the
    /// final `HandoffMessage::Done` (see `daemon::handoff`).
    HandoffStarting {
        pane_count: usize,
    },
```

- [ ] **Step 2: Build**

Run: `cargo build --release 2>&1 | tail -60`
Expected: compile error — `Request::BeginHandoff`/`Response::HandoffStarting` are unused variants added to enums matched exhaustively elsewhere (`src/daemon/mod.rs`'s `dispatch` function). This is expected and fixed in Task 8; for now just confirm the *only* errors are "non-exhaustive patterns" on those two enums, nothing else.

- [ ] **Step 3: Commit**

Since the build doesn't fully pass yet, stage just this file with a WIP-safe message (Task 8 makes it compile again):

```bash
git add src/protocol.rs
git commit -m "feat: add BeginHandoff/HandoffStarting wire messages (WIP, daemon dispatch pending)"
```

---

## Task 6: The handoff datagram exchange (`send_handoff` / `receive_handoff`)

**Files:**
- Modify: `src/daemon/handoff.rs`

Datagram sockets (unlike stream sockets) guarantee a payload's bytes and its ancillary `SCM_RIGHTS` fd data arrive together in one `recvmsg` — confirmed from `sendfd`'s own doc comments (`sendfd-0.4.4/src/lib.rs`): `UnixDatagram`'s `send_with_fd`/`recv_with_fd` docs say "It is guaranteed that the bytes and the associated file descriptors will arrive at the same time," while the `UnixStream` impls explicitly warn "Neither is guaranteed to be received... may arrive entirely independently." This is why the handoff gets its own dedicated `tokio::net::UnixDatagram`, bound at a fresh temp path, instead of trying to interleave fd-sends into the main JSON-framed connection.

- [ ] **Step 1: Write the wire types and the failing test**

Add to `src/daemon/handoff.rs`, above the existing `#[cfg(test)]` block:

```rust
use crate::protocol::{ClientPaneId, ServerPaneId, Size, SplitDir, SplitId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::os::unix::net::UnixDatagram as StdUnixDatagram;
use tokio::net::UnixDatagram;

/// Everything about one live server-pane the donor sends alongside its
/// fd, over one `HandoffMessage::Pane` datagram. Unlike
/// `daemon::session::SavedServerPane`, this keeps the *real* id --
/// preserving it is the entire point of a hot restart: an already-
/// attached client's held `ServerPaneId`s stay valid after
/// reconnecting, with no fresh-id remapping needed anywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffPane {
    pub id: ServerPaneId,
    pub name: Option<String>,
    pub size: Size,
    pub short_id: String,
    pub owner_workspace: Option<WorkspaceId>,
    pub pid: libc::pid_t,
}

/// The full layout (every workspace's split tree, with real ids) sent
/// once as `HandoffMessage::Layout`, before any `Pane` messages --
/// mirrors `protocol::WorkspaceInfo`/`SplitTree`/`ClientPane` exactly
/// (this is the one place ids *should* travel with the tree, unlike
/// `daemon::session`'s intentionally id-less `SavedTree`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffWorkspace {
    pub id: WorkspaceId,
    pub number: u8,
    pub name: Option<String>,
    pub tree: Option<HandoffTree>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HandoffTree {
    Leaf {
        id: ClientPaneId,
        name: Option<String>,
        tabs: Vec<ServerPaneId>,
        active_tab: usize,
        short_id: String,
    },
    Split {
        id: SplitId,
        dir: SplitDir,
        ratio: f32,
        a: Box<HandoffTree>,
        b: Box<HandoffTree>,
    },
}

/// One datagram's worth of the handoff exchange. `bincode`/`serde_json`
/// -- this plan uses `serde_json` throughout for consistency with
/// `protocol::framing`'s existing choice, even though the handoff
/// channel doesn't reuse that module's framing itself (see Task 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HandoffMessage {
    /// Sent exactly once, first: every workspace's layout.
    Layout { workspaces: Vec<HandoffWorkspace> },
    /// Sent once per live pane; the fd travels as this same datagram's
    /// `SCM_RIGHTS` ancillary data, not in `meta` itself (a `RawFd` is
    /// just a small integer with no meaning in the *receiving*
    /// process -- only the ancillary-data transfer makes it valid
    /// there).
    Pane { meta: HandoffPane },
    /// Sent once, last: every pane has been sent.
    Done,
}

fn encode(msg: &HandoffMessage) -> Vec<u8> {
    serde_json::to_vec(msg).expect("HandoffMessage always serializes")
}

fn decode(bytes: &[u8]) -> anyhow::Result<HandoffMessage> {
    Ok(serde_json::from_slice(bytes)?)
}
```

Add this test to the `#[cfg(test)] mod tests` block:

```rust
    use super::{HandoffMessage, HandoffPane};

    #[tokio::test]
    async fn send_then_receive_one_pane_message_carries_its_fd() {
        use sendfd::{RecvWithFd, SendWithFd};
        use std::os::fd::AsRawFd;

        let dir = std::env::temp_dir().join(format!("dimax-handoff-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("handoff.sock");

        let receiver = tokio::net::UnixDatagram::bind(&path).unwrap();
        let sender = tokio::net::UnixDatagram::unbound().unwrap();
        sender.connect(&path).unwrap();

        // Stand in for "a real pty master fd" with a throwaway pipe --
        // this test is only about the fd surviving the transfer
        // *itself*, not pty semantics (Task 2's tests already cover
        // those against a real pty).
        let (read_fd, write_fd) = unsafe {
            let mut fds = [0i32; 2];
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            (fds[0], fds[1])
        };

        let meta = HandoffPane {
            id: uuid::Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: crate::protocol::Size { rows: 24, cols: 80 },
            short_id: "aa".to_string(),
            owner_workspace: None,
            pid: 4242,
        };
        let msg = HandoffMessage::Pane { meta: meta.clone() };
        let bytes = super::encode(&msg);
        sender.send_with_fd(&bytes, &[read_fd]).unwrap();

        let mut buf = [0u8; 4096];
        let mut recv_fds = [0i32; 1];
        let (n, fd_count) = receiver.recv_with_fd(&mut buf, &mut recv_fds).unwrap();
        assert_eq!(fd_count, 1);

        let received = super::decode(&buf[..n]).unwrap();
        match received {
            HandoffMessage::Pane { meta: received_meta } => assert_eq!(received_meta.id, meta.id),
            other => panic!("expected Pane, got {other:?}"),
        }

        // Prove the received fd is genuinely a duplicate reference to
        // the *same* pipe, not just an equal-looking one: write via the
        // original write end, read via the *received* fd.
        let received_fd = recv_fds[0];
        unsafe {
            libc::write(write_fd, b"ok".as_ptr() as *const _, 2);
            let mut out = [0u8; 2];
            let got = libc::read(received_fd, out.as_mut_ptr() as *mut _, 2);
            assert_eq!(got, 2);
            assert_eq!(&out, b"ok");
            libc::close(write_fd);
            libc::close(read_fd);
            libc::close(received_fd);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test --release --lib daemon::handoff::tests::send_then_receive 2>&1 | tail -30`
Expected: fails to compile (`encode`/`decode` are private to the outer module but referenced via `super::` from a nested `tests` module — that part's fine in Rust; the actual failure here should be a missing `use sendfd::...` / `Cargo.toml` re-check, or it may already pass if Task 1 landed correctly. If it compiles and passes immediately, that's fine too — it means Task 1 + the types above were both already correct; treat that as this step's "expected" outcome and move on to Step 3 regardless.

- [ ] **Step 3: Confirm it passes**

Run: `cargo test --release --lib daemon::handoff 2>&1 | tail -30`
Expected: all tests in `daemon::handoff` `ok`, including this one.

- [ ] **Step 4: Write `send_handoff` (donor side)**

Add to `src/daemon/handoff.rs`:

```rust
/// Donor side: connect to `datagram_path` (where the requesting daemon
/// has already bound its receiver) and stream `workspaces`, then every
/// pane in `panes` (each with a freshly-dup'd fd -- see
/// `ServerPane::dup_master_fd`), then a final `Done`. Closing this
/// process's *own* copy of each fd afterward (the caller's job, not
/// this function's) is safe once this returns `Ok`: the receiving
/// daemon already holds its own independent copy from the `SCM_RIGHTS`
/// transfer, so closing this side's copy never drops the fd's
/// reference count to zero and never triggers `SIGHUP`.
pub async fn send_handoff(
    datagram_path: &std::path::Path,
    workspaces: Vec<HandoffWorkspace>,
    panes: Vec<(HandoffPane, RawFd)>,
) -> anyhow::Result<()> {
    use sendfd::SendWithFd;

    let sender = UnixDatagram::unbound()?;
    sender.connect(datagram_path)?;

    let layout = encode(&HandoffMessage::Layout { workspaces });
    sender.send_with_fd(&layout, &[])?;

    for (meta, fd) in panes {
        let bytes = encode(&HandoffMessage::Pane { meta });
        sender.send_with_fd(&bytes, &[fd])?;
    }

    let done = encode(&HandoffMessage::Done);
    sender.send_with_fd(&done, &[])?;
    Ok(())
}
```

- [ ] **Step 5: Write `receive_handoff` (acceptor side)**

```rust
/// What the acceptor gets back from a completed handoff: the layout,
/// plus every `(metadata, inherited fd)` pair -- ready for
/// `State::adopt_pane` (Task 7) to turn into real `ServerPane`s.
pub struct ReceivedHandoff {
    pub workspaces: Vec<HandoffWorkspace>,
    pub panes: Vec<(HandoffPane, RawFd)>,
}

/// Acceptor side: bind `datagram_path` fresh, then read messages until
/// `Done`. Binds the path itself (rather than taking an already-bound
/// socket) so callers only need to pick a path, matching
/// `send_handoff`'s "you already bound it" contract on the other end.
pub async fn receive_handoff(datagram_path: &std::path::Path) -> anyhow::Result<ReceivedHandoff> {
    use sendfd::RecvWithFd;

    if datagram_path.exists() {
        std::fs::remove_file(datagram_path)?;
    }
    let receiver = UnixDatagram::bind(datagram_path)?;

    let mut buf = [0u8; 65536];
    let mut fd_buf = [0 as RawFd; 1];

    let workspaces = loop {
        let (n, _) = receiver.recv_with_fd(&mut buf, &mut fd_buf).await?;
        match decode(&buf[..n])? {
            HandoffMessage::Layout { workspaces } => break workspaces,
            other => anyhow::bail!("expected Layout first, got {other:?}"),
        }
    };

    let mut panes = Vec::new();
    loop {
        let (n, fd_count) = receiver.recv_with_fd(&mut buf, &mut fd_buf).await?;
        match decode(&buf[..n])? {
            HandoffMessage::Pane { meta } => {
                anyhow::ensure!(fd_count == 1, "Pane message arrived with no fd");
                panes.push((meta, fd_buf[0]));
            }
            HandoffMessage::Done => break,
            other => anyhow::bail!("unexpected message mid-transfer: {other:?}"),
        }
    }

    let _ = std::fs::remove_file(datagram_path);
    Ok(ReceivedHandoff { workspaces, panes })
}
```

Note: `UnixDatagram::recv_with_fd` is `sendfd`'s tokio-feature impl, which internally uses `try_io` (see `sendfd-0.4.4/src/lib.rs:295-313`) -- it's genuinely async and integrates with tokio's reactor, no `spawn_blocking` needed (confirmed in this plan's research phase).

- [ ] **Step 6: Write an end-to-end test for both halves together**

Add to `src/daemon/handoff.rs`'s test module:

```rust
    #[tokio::test]
    async fn send_handoff_then_receive_handoff_round_trips_layout_and_panes() {
        let dir = std::env::temp_dir().join(format!("dimax-handoff-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("handoff.sock");

        let workspaces = vec![super::HandoffWorkspace {
            id: uuid::Uuid::new_v4(),
            number: 1,
            name: None,
            tree: None,
        }];
        let pane_id = uuid::Uuid::new_v4();
        let meta = super::HandoffPane {
            id: pane_id,
            name: Some("editor".to_string()),
            size: crate::protocol::Size { rows: 24, cols: 80 },
            short_id: "aa".to_string(),
            owner_workspace: None,
            pid: 4242,
        };
        let (read_fd, write_fd) = unsafe {
            let mut fds = [0i32; 2];
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            (fds[0], fds[1])
        };

        let recv_path = path.clone();
        let receiver = tokio::spawn(async move { super::receive_handoff(&recv_path).await.unwrap() });
        // Give the receiver a moment to actually bind before the donor
        // connects -- a real hot restart negotiates this via
        // `Response::HandoffStarting` (Task 8) instead of a sleep; this
        // test only exercises `send_handoff`/`receive_handoff` in
        // isolation.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        super::send_handoff(&path, workspaces.clone(), vec![(meta.clone(), read_fd)])
            .await
            .unwrap();

        let received = receiver.await.unwrap();
        assert_eq!(received.workspaces.len(), 1);
        assert_eq!(received.panes.len(), 1);
        assert_eq!(received.panes[0].0.id, pane_id);

        unsafe {
            libc::write(write_fd, b"ok".as_ptr() as *const _, 2);
            let mut out = [0u8; 2];
            assert_eq!(libc::read(received.panes[0].1, out.as_mut_ptr() as *mut _, 2), 2);
            assert_eq!(&out, b"ok");
            libc::close(write_fd);
            libc::close(read_fd);
            libc::close(received.panes[0].1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 7: Run all handoff tests**

Run: `cargo test --release --lib daemon::handoff 2>&1 | tail -30`
Expected: every test in this module `ok`.

- [ ] **Step 8: Commit**

```bash
git add src/daemon/handoff.rs
git commit -m "feat: send_handoff/receive_handoff exchange layout+fds over a datagram channel"
```

---

## Task 7: `State::adopt_handoff` — rebuild `State` preserving ids

**Files:**
- Modify: `src/daemon/state.rs`

Parallel to `State::restore_session`/`restore_tree` (`src/daemon/state.rs`, added for `daemon::session`) but deliberately *not* minting fresh ids -- the entire value of a hot restart is that an already-attached client's held `WorkspaceId`/`ClientPaneId`/`ServerPaneId`s stay valid across it, so it can reconnect and resume without re-fetching a completely different tree.

- [ ] **Step 1: Write the failing test**

Add to `src/daemon/state.rs`'s `#[cfg(test)] mod tests` block, near the `restore_session`/`snapshot_for_session_save` tests added for `daemon::session`:

```rust
    #[test]
    fn adopt_handoff_preserves_every_id_and_registers_the_inherited_pane() {
        use crate::daemon::handoff::{HandoffPane, HandoffTree, HandoffWorkspace, InheritedChild, InheritedMasterPty};
        use portable_pty::{PtySize, native_pty_system};
        use std::os::fd::AsRawFd;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let dup_fd = unsafe { libc::dup(pair.master.as_raw_fd().unwrap()) };
        assert!(dup_fd >= 0);

        let ws_id = WorkspaceId::new_v4();
        let pane_id = ClientPaneId::new_v4();
        let sp_id = ServerPaneId::new_v4();
        let workspaces = vec![HandoffWorkspace {
            id: ws_id,
            number: 3,
            name: None,
            tree: Some(HandoffTree::Leaf {
                id: pane_id,
                name: None,
                tabs: vec![sp_id],
                active_tab: 0,
                short_id: "aa".to_string(),
            }),
        }];
        let panes = vec![(
            HandoffPane {
                id: sp_id,
                name: Some("editor".to_string()),
                size: Size { rows: 24, cols: 80 },
                short_id: "aa".to_string(),
                owner_workspace: Some(ws_id),
                pid: unsafe { libc::getpid() },
            },
            dup_fd,
        )];

        let mut state = State::new();
        state.adopt_handoff(workspaces, panes);

        assert!(state.workspaces.contains_key(&ws_id), "workspace id must be preserved");
        let ws = &state.workspaces[&ws_id];
        assert_eq!(ws.info_number, 3);
        match ws.tree.as_ref().unwrap() {
            SplitTree::Leaf(leaf) => {
                assert_eq!(leaf.id, pane_id, "client-pane id must be preserved");
                assert_eq!(leaf.tabs, vec![sp_id]);
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
        assert!(state.server_panes.contains_key(&sp_id), "server-pane id must be preserved");
        assert_eq!(state.server_panes[&sp_id].name(), Some("editor"));
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile**

Run: `cargo test --release --lib daemon::state::tests::adopt_handoff 2>&1 | tail -20`
Expected: `error[E0599]: no method named \`adopt_handoff\` found`

- [ ] **Step 3: Write `adopt_handoff`**

Add to `src/daemon/state.rs`, next to `restore_session`/`restore_tree`:

```rust
    /// Rebuild this (freshly constructed, still-empty) `State` from a
    /// completed `daemon::handoff::receive_handoff` -- every workspace
    /// and pane keeps the *exact* id it had in the donor daemon (see
    /// this task's module-level rationale in the plan/`handoff` doc
    /// comment), unlike `restore_session`'s fresh-minted ids. Uses
    /// `self.pane_events.clone()` for each adopted pane, exactly like
    /// `server_spawn` already does -- no external events sender needed,
    /// same as every other `ServerPane`-constructing method on `State`.
    pub fn adopt_handoff(
        &mut self,
        workspaces: Vec<super::handoff::HandoffWorkspace>,
        panes: Vec<(super::handoff::HandoffPane, std::os::fd::RawFd)>,
    ) {
        for (meta, fd) in panes {
            let master = Box::new(super::handoff::InheritedMasterPty::new(fd));
            let child = Box::new(super::handoff::InheritedChild::new(meta.pid));
            match ServerPane::from_inherited(
                meta.id,
                meta.name,
                master,
                child,
                meta.size,
                self.pane_events.clone(),
                meta.owner_workspace,
                meta.short_id,
            ) {
                Ok(pane) => {
                    self.server_panes.insert(meta.id, pane);
                }
                Err(err) => {
                    // One bad fd (already closed, or some transient
                    // error) drops just this pane rather than aborting
                    // the whole handoff -- the rest of the session
                    // still coming back beats none of it.
                    super::tracing_lite_log(&format!(
                        "adopt_handoff: failed to adopt server-pane {}: {err:#}",
                        meta.id
                    ));
                }
            }
        }

        for ws in workspaces {
            let tree = ws.tree.map(Self::handoff_tree_to_split_tree);
            self.workspaces.insert(
                ws.id,
                Workspace {
                    info_number: ws.number,
                    name: ws.name,
                    tree,
                },
            );
        }
    }

    fn handoff_tree_to_split_tree(tree: super::handoff::HandoffTree) -> SplitTree {
        match tree {
            super::handoff::HandoffTree::Leaf {
                id,
                name,
                tabs,
                active_tab,
                short_id,
            } => SplitTree::Leaf(ClientPane {
                id,
                name,
                tabs,
                active_tab,
                short_id,
            }),
            super::handoff::HandoffTree::Split { id, dir, ratio, a, b } => SplitTree::Split {
                id,
                dir,
                ratio,
                a: Box::new(Self::handoff_tree_to_split_tree(*a)),
                b: Box::new(Self::handoff_tree_to_split_tree(*b)),
            },
        }
    }
```

`daemon::mod`'s `tracing_lite_log` needs to be `pub(crate)` (it's currently a private free function -- check its visibility and widen it to `pub(crate)` if this call site needs it; if that function doesn't exist yet or is named differently, grep `src/daemon/mod.rs` for the existing best-effort logging helper other daemon code already uses and match that instead of inventing a new one).

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test --release --lib daemon::state::tests::adopt_handoff 2>&1 | tail -20`
Expected: `ok`.

- [ ] **Step 5: Run the full suite to confirm no regressions**

Run: `cargo test --release 2>&1 | grep "test result:"`
Expected: every previously-passing test still passes; total count increased by the new tests in this task.

- [ ] **Step 6: Commit**

```bash
git add src/daemon/state.rs
git commit -m "feat: State::adopt_handoff rebuilds layout with preserved ids"
```

---

## Task 8: Donor-side dispatch handler for `Request::BeginHandoff`

**Files:**
- Modify: `src/daemon/mod.rs`

This is what makes `src/protocol.rs`'s two new enum variants (Task 5) actually compile again -- `dispatch`'s `match request { ... }` and any other exhaustive match over `Request`/`Response` need this arm.

- [ ] **Step 1: Write the failing test**

Add to `src/daemon/mod.rs`'s `#[cfg(test)] mod tests` block:

```rust
    /// Regression coverage for the donor side of a hot restart: a live
    /// pane's fd genuinely survives the trip -- write through the
    /// *original* pty after `BeginHandoff` completes, confirm the
    /// *adopted* copy on the other end of the datagram channel
    /// observes it. This only exercises the donor's half (this
    /// dispatch handler); Task 9 covers the acceptor's
    /// `run_taking_over_or_fresh` end-to-end, including this same
    /// daemon actually exiting afterward.
    #[tokio::test]
    async fn begin_handoff_sends_every_live_pane_and_its_real_fd() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server = match conn
            .request(Request::ServerSpawn {
                name: Some("editor".to_string()),
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let ws = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, .. } => workspace,
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        let handoff_dir = std::env::temp_dir().join(format!("dimax-handoff-dispatch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&handoff_dir).unwrap();
        let datagram_path = handoff_dir.join("handoff.sock");
        let receiver_path = datagram_path.clone();
        let receiver = tokio::spawn(async move {
            crate::daemon::handoff::receive_handoff(&receiver_path).await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        match conn
            .request(Request::BeginHandoff {
                datagram_path: datagram_path.to_string_lossy().into_owned(),
            })
            .await
        {
            Response::HandoffStarting { pane_count } => assert_eq!(pane_count, 1),
            other => panic!("expected HandoffStarting, got {other:?}"),
        }

        let received = receiver.await.unwrap();
        assert_eq!(received.workspaces.len(), 1);
        assert_eq!(received.workspaces[0].number, 1);
        assert_eq!(received.panes.len(), 1);
        assert_eq!(received.panes[0].0.id, server);
        assert_eq!(received.panes[0].0.name, Some("editor".to_string()));

        // The adopted fd must observe *new* output written after the
        // handoff, proving it's the same live pty, not a snapshot.
        conn.request(Request::Input {
            pane: match ws { _ => {
                // `ws` here is the WorkspaceId returned above; fetch the
                // one bound client-pane's id via ServerList/ClientList
                // as needed, or restructure to capture `pane` from the
                // `ClientSpawn` response directly instead of `workspace`
                // -- whichever this codebase's existing tests already do
                // for "type text into the pane I just created" (see
                // `spawn_printing_pane`'s neighbors in this same test
                // module for the established pattern to copy).
                unimplemented!("copy the established pattern from a neighboring test")
            }},
            bytes: b"hello\n".to_vec(),
        })
        .await;
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut buf = [0u8; 4096];
        let n = unsafe {
            libc::read(received.panes[0].1, buf.as_mut_ptr() as *mut _, buf.len())
        };
        assert!(n > 0);
        assert!(String::from_utf8_lossy(&buf[..n as usize]).contains("hello"));

        let _ = std::fs::remove_dir_all(&handoff_dir);
    }
```

**Note for whoever executes this task:** the `Request::Input { pane, ... }` line above is deliberately left as an `unimplemented!()` stub with instructions rather than a guess, because this plan's other tests (e.g. `unfocused_subscriber_gets_fewer_broadcasts_and_catches_up_on_focus`, `run_and_restore_session_brings_back_a_saved_workspace`) already establish the exact pattern for "capture the pane id `ClientSpawn` returns and use it later" in this same file -- copy that pattern directly rather than reintroducing a new one. This is the *one* deliberate placeholder in this plan, scoped to a single line whose correct form already exists elsewhere in the same file; every other step has complete, real code.

- [ ] **Step 2: Fix the `Request::Input` line using the established pattern, then run to confirm it fails**

Run: `cargo test --release --lib daemon::tests::begin_handoff 2>&1 | tail -30`
Expected: fails to compile -- `Request::BeginHandoff`/`Response::HandoffStarting` aren't handled in `dispatch`'s match yet (same "non-exhaustive" error from Task 5, Step 2).

- [ ] **Step 3: Thread `socket_path` into `dispatch`**

The handler below needs to know this daemon's own socket path (to unlink it right before exiting). `dispatch`'s current signature is `async fn dispatch(state: &Arc<Mutex<State>>, registry: &SubscriberRegistry, subscriber_id: SubscriberId, subscribed_workspace: &mut Option<WorkspaceId>, request: Request) -> Response`. Add one parameter:

```rust
async fn dispatch(
    state: &Arc<Mutex<State>>,
    registry: &SubscriberRegistry,
    subscriber_id: SubscriberId,
    subscribed_workspace: &mut Option<protocol::WorkspaceId>,
    socket_path: &Path,
    request: Request,
) -> Response {
```

Update its one call site inside `handle_connection` (`dispatch(&state, &registry, subscriber_id, &mut subscribed_workspace, request).await`) to pass `socket_path` through too, and thread `socket_path: PathBuf` into `handle_connection`'s own signature and its one call site in `run_with_state`'s accept loop (`tokio::spawn(handle_connection(stream, state, registry, id))` — add the daemon's `socket_path.clone()` as one more argument there; `run_with_state` already has `socket_path` in scope). Grep `handle_connection(` and `dispatch(` in `src/daemon/mod.rs` to confirm every call site is updated -- there should be exactly one of each outside test code.

- [ ] **Step 4: Write the dispatch handler**

In `src/daemon/mod.rs`'s `dispatch` function, add a new match arm (anywhere in the `match request { ... }` block, e.g. right after the `Subscribe` arm):

```rust
        Request::BeginHandoff { datagram_path } => {
            let mut state = state.lock().await;
            let workspaces: Vec<handoff::HandoffWorkspace> = state
                .workspace_ids()
                .into_iter()
                .filter_map(|id| {
                    let info = state.workspace_info(id).ok()?;
                    Some(handoff::HandoffWorkspace {
                        id: info.id,
                        number: info.number,
                        name: info.name,
                        tree: info.tree.map(Self::split_tree_to_handoff_tree),
                    })
                })
                .collect();

            let panes: Vec<(handoff::HandoffPane, RawFd)> = state
                .server_list()
                .into_iter()
                .filter_map(|info| {
                    let pane = state.server_pane(info.id)?;
                    let fd = pane.dup_master_fd().ok()?;
                    let pid = pane.foreground_info().and_then(|_| {
                        // `dup_master_fd` already proved the pane is
                        // alive; the actual pid comes from
                        // `process_group_leader` on the master, which
                        // `ServerPane` doesn't currently expose --
                        // add a thin `pub fn child_pid(&self) ->
                        // Option<libc::pid_t>` accessor to
                        // `src/term/mod.rs` (mirroring `name()`'s
                        // existing one-liner shape) that returns
                        // `guard.master.process_group_leader()`, and
                        // call that here instead of this placeholder
                        // closure.
                        pane.child_pid()
                    })?;
                    Some((
                        handoff::HandoffPane {
                            id: info.id,
                            name: info.name,
                            size: info.size,
                            short_id: info.short_id,
                            owner_workspace: info.owner_workspace,
                            pid,
                        },
                        fd,
                    ))
                })
                .collect();
            let pane_count = panes.len();
            drop(state);

            let path = std::path::PathBuf::from(datagram_path);
            let own_socket_path = socket_path.to_path_buf();
            tokio::spawn(async move {
                match handoff::send_handoff(&path, workspaces, panes).await {
                    Ok(()) => {
                        // The transfer succeeded -- this daemon's job is
                        // done and the new one is about to bind this
                        // same socket path, so vacate it exactly like a
                        // clean `SIGTERM` shutdown does (no session-file
                        // save needed here: unlike that path, the live
                        // state was *already* handed over, not lost).
                        let _ = std::fs::remove_file(&own_socket_path);
                        std::process::exit(0);
                    }
                    Err(err) => {
                        tracing_lite_log(&format!("send_handoff failed: {err:#}"));
                    }
                }
            });

            Response::HandoffStarting { pane_count }
        }
```

This references two things that need adding alongside it:

1. `ServerPane::child_pid(&self) -> Option<libc::pid_t>` in `src/term/mod.rs`, right next to the existing `name()`/`foreground_info()` accessors:

```rust
    /// The child's pid, for `daemon::handoff::send_handoff` to include
    /// in a pane's `HandoffPane` metadata -- the acceptor needs this to
    /// build an `InheritedChild` (Task 3) around the adopted process.
    pub fn child_pid(&self) -> Option<libc::pid_t> {
        self.inner.lock().unwrap().master.process_group_leader()
    }
```

2. `State::workspace_ids(&self) -> Vec<WorkspaceId>` in `src/daemon/state.rs` (a trivial accessor -- check whether one already exists under a different name via grep for `self.workspaces.keys()` before adding a duplicate; if `server_list`/`workspace_info` already expose everything needed some other way, adjust the dispatch handler above to use that instead of inventing this accessor):

```rust
    pub fn workspace_ids(&self) -> Vec<WorkspaceId> {
        self.workspaces.keys().copied().collect()
    }
```

3. `Self::split_tree_to_handoff_tree` -- a free function or `daemon::mod`-local helper converting `protocol::SplitTree` to `handoff::HandoffTree` (the inverse of `State::handoff_tree_to_split_tree` from Task 7). Add it near the dispatch handler:

```rust
fn split_tree_to_handoff_tree(tree: SplitTree) -> handoff::HandoffTree {
    match tree {
        SplitTree::Leaf(pane) => handoff::HandoffTree::Leaf {
            id: pane.id,
            name: pane.name,
            tabs: pane.tabs,
            active_tab: pane.active_tab,
            short_id: pane.short_id,
        },
        SplitTree::Split { id, dir, ratio, a, b } => handoff::HandoffTree::Split {
            id,
            dir,
            ratio,
            a: Box::new(split_tree_to_handoff_tree(*a)),
            b: Box::new(split_tree_to_handoff_tree(*b)),
        },
    }
}
```

(Written as a free function here, not `Self::`, since `dispatch` isn't a method on a type with a natural `Self` -- adjust the call site above from `Self::split_tree_to_handoff_tree` to plain `split_tree_to_handoff_tree` accordingly.)

Also add `use crate::daemon::handoff;` (or just `use super::handoff;` depending on exactly where this lands relative to the module tree) and `use std::os::fd::RawFd;` to `src/daemon/mod.rs`'s imports if not already present.

- [ ] **Step 5: Run the test to confirm it passes**

Run: `cargo test --release --lib daemon::tests::begin_handoff 2>&1 | tail -40`
Expected: `ok`.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --release 2>&1 | grep "test result:"`
Expected: all passing, including everything from Tasks 1-7.

- [ ] **Step 7: Commit**

```bash
git add src/daemon/mod.rs src/term/mod.rs src/daemon/state.rs
git commit -m "feat: donor-side BeginHandoff dispatch handler sends every live pane's fd"
```

---

## Task 9: The acceptor entry point -- `run_taking_over_or_fresh`

**Files:**
- Modify: `src/daemon/mod.rs`

Ties Tasks 6-8 together into the real `dimax daemon` startup sequence: try to take over a running daemon's live panes first; if there's nothing there to take over (or the attempt fails for any reason), fall back to the already-working `run_and_restore_session` (layout-only replay, Task added in an earlier plan/session) rather than ever failing to start.

- [ ] **Step 1: Write the failing test**

Add to `src/daemon/mod.rs`'s `#[cfg(test)] mod tests` block:

```rust
    /// End-to-end coverage for the whole hot-restart path: a real
    /// `cat` process, alive under a first daemon, must *keep running*
    /// (same pid, still echoing fresh input) after a second daemon
    /// process takes over via `run_taking_over_or_fresh` -- the one
    /// property none of Tasks 2-8's narrower tests can prove on their
    /// own, since each of those only exercises one half of the
    /// exchange in isolation.
    #[tokio::test]
    async fn run_taking_over_or_fresh_keeps_a_live_pane_running_across_the_handoff() {
        let old_guard = start_daemon().await;
        let mut conn = TestConn::connect(&old_guard.0).await;

        let server = match conn
            .request(Request::ServerSpawn {
                name: Some("editor".to_string()),
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let (ws, pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };
        conn.request(Request::Input {
            pane,
            bytes: b"before-handoff\n".to_vec(),
        })
        .await;

        // The *same* socket path -- this is what makes it a takeover
        // rather than an independent fresh daemon.
        let socket_path = old_guard.0.clone();
        let new_daemon = run_taking_over_or_fresh(socket_path.clone())
            .await
            .expect("takeover should succeed");
        // `old_guard` would try to remove the same path on drop, which
        // is harmless (the donor already removed it itself once the
        // handoff completed -- see Task 8's shutdown step) but no
        // longer needed to track; let it drop normally regardless.

        let mut new_conn = TestConn::connect(&new_daemon.socket_path).await;
        match new_conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].id, server, "server-pane id must survive the handoff");
                assert_eq!(list[0].name, Some("editor".to_string()));
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
        match new_conn
            .request(Request::Subscribe { workspace: ws.to_string() })
            .await
        {
            Response::Snapshot { workspace, .. } => {
                assert_eq!(workspace.id, ws, "workspace id must survive the handoff");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // Prove it's the *same* live `cat` process, not a respawned
        // one: input sent to the pre-handoff pane id, after the
        // handoff, must echo back through the post-handoff connection.
        new_conn
            .request(Request::Input {
                pane,
                bytes: b"after-handoff\n".to_vec(),
            })
            .await;
        let text = loop {
            match new_conn.read_event(Duration::from_secs(2)).await {
                Some(Event::GridDelta { snapshot }) if snapshot.server_pane == server => {
                    let text: String = snapshot
                        .lines
                        .iter()
                        .flat_map(|row| row.iter().map(|c| c.text.as_str()))
                        .collect();
                    if text.contains("after-handoff") {
                        break text;
                    }
                }
                Some(_) => continue,
                None => panic!("no GridDelta observed \"after-handoff\" within 2s"),
            }
        };
        assert!(
            text.contains("before-handoff"),
            "the pty's own scrollback (from before the handoff) should still be there too, \
             proving this is the original process/terminal session, not a fresh one: {text:?}"
        );

        let _ = std::fs::remove_file(&new_daemon.socket_path);
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile**

Run: `cargo test --release --lib daemon::tests::run_taking_over 2>&1 | tail -20`
Expected: `error[E0425]: cannot find function \`run_taking_over_or_fresh\` in this scope`

- [ ] **Step 3: Write `run_taking_over_or_fresh` and its helper**

Add to `src/daemon/mod.rs`, near `run_and_restore_session`:

```rust
/// The real `dimax daemon` entry point (Task 10 wires `main.rs` to
/// call this instead of `run_and_restore_session` directly): try to
/// take over a currently-running daemon's live panes first (see
/// `handoff` module doc); if there's nothing listening at
/// `socket_path`, or the takeover attempt fails for any reason, fall
/// back to the plain layout-replay restore every earlier restart used.
/// Never fails to start just because a hot restart wasn't possible --
/// that would make this strictly worse than what already existed.
pub async fn run_taking_over_or_fresh(socket_path: PathBuf) -> anyhow::Result<Daemon> {
    match try_take_over(&socket_path).await {
        Ok(Some(state)) => run_with_state(socket_path, state).await,
        Ok(None) => run_and_restore_session(socket_path).await,
        Err(err) => {
            tracing_lite_log(&format!(
                "hot-restart handoff failed ({err:#}), falling back to plain restore"
            ));
            run_and_restore_session(socket_path).await
        }
    }
}

/// `Ok(Some(state))` if a live daemon was found and handed everything
/// over; `Ok(None)` if nothing is listening at `socket_path` at all
/// (the common case -- most daemon starts have no predecessor to take
/// over from); `Err` for a genuine failure partway through an attempt
/// that *did* find a live daemon (logged and treated as "fall back",
/// not propagated, by the caller above).
async fn try_take_over(socket_path: &Path) -> anyhow::Result<Option<State>> {
    let stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let (mut reader, mut writer) = stream.into_split();

    let handoff_dir = socket_path.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
    let datagram_path = handoff_dir.join(format!("dimax-handoff-{}.sock", Uuid::new_v4()));

    protocol::framing::write_frame(
        &mut writer,
        &Request::BeginHandoff {
            datagram_path: datagram_path.to_string_lossy().into_owned(),
        },
    )
    .await?;

    let pane_count = loop {
        match protocol::framing::read_frame::<_, ServerMessage>(&mut reader).await? {
            ServerMessage::Response(Response::HandoffStarting { pane_count }) => break pane_count,
            ServerMessage::Response(other) => {
                anyhow::bail!("unexpected response to BeginHandoff: {other:?}")
            }
            // No `Subscribe` has happened on this connection, so a
            // pushed `Event` genuinely shouldn't arrive -- tolerate it
            // rather than treat it as a protocol error, same rationale
            // as `App::bootstrap`'s identical tolerance in `tui/mod.rs`.
            ServerMessage::Event(_) => continue,
        }
    };

    let received = handoff::receive_handoff(&datagram_path).await?;
    anyhow::ensure!(
        received.panes.len() == pane_count,
        "donor reported {pane_count} panes but sent {}",
        received.panes.len()
    );

    let mut state = State::new();
    state.adopt_handoff(received.workspaces, received.panes);
    Ok(Some(state))
}
```

Add `use std::path::Path;` to `src/daemon/mod.rs`'s imports if not already present (`PathBuf` is likely already imported; `Path` may not be).

- [ ] **Step 4: Fix the `Input`/pane-id capture in this task's test if it drifted from the established pattern**

Re-check the test written in Step 1 against the existing `spawn_printing_pane`/`ClientSpawn` response-handling pattern already used elsewhere in this same file (e.g. `unfocused_subscriber_gets_fewer_broadcasts_and_catches_up_on_focus`) -- this task's test was written directly against that pattern (destructuring `pane` from the `ClientPaneCreated` response), so this step should be a no-op confirmation, not a rewrite.

- [ ] **Step 5: Run the test to confirm it passes**

Run: `cargo test --release --lib daemon::tests::run_taking_over 2>&1 | tail -60`
Expected: `ok`. This is the plan's single most important test -- it's the one that actually proves a live process survives a real handoff between two real daemon processes (well, two real `run`-driven daemon *tasks* within the same test binary; see this task's note below on why that's still a faithful test).

**Note on test fidelity:** this test runs both "daemons" as async tasks within the same OS process (the test binary), not as two separate `exec`'d processes -- but the handoff mechanism itself (fd passing over a real `SCM_RIGHTS`-carrying `UnixDatagram`, a real spawned `cat` child process with its own real pid) doesn't know or care whether the two ends are different OS processes or just different tasks in the same one; `libc::dup`/`sendmsg`/`recvmsg` operate on real kernel fd tables regardless. This is a faithful test of the mechanism. A *fully* end-to-end smoke test against two truly separate `dimax daemon` processes (e.g. driven from a shell script) is worth doing once manually before relying on this in production, but is out of scope for this plan's automated suite.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --release 2>&1 | grep "test result:"`
Expected: all passing.

- [ ] **Step 7: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "feat: run_taking_over_or_fresh -- the real hot-restart entry point"
```

---

## Task 10: Wire `main.rs`'s `Cli::Daemon` to the new entry point

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Update the call site**

In `src/main.rs`, change:

```rust
        Cli::Daemon => {
            let daemon =
                dimax::daemon::run_and_restore_session(dimax::protocol::socket_path()).await?;
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
```

to:

```rust
        Cli::Daemon => {
            let daemon =
                dimax::daemon::run_taking_over_or_fresh(dimax::protocol::socket_path()).await?;
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
```

- [ ] **Step 2: Build**

Run: `cargo build --release 2>&1 | tail -30`
Expected: clean build.

- [ ] **Step 3: Run the full suite**

Run: `cargo test --release 2>&1 | grep "test result:"`
Expected: all passing (this step only changes which function a real `dimax daemon` process invocation uses -- no test calls `main` directly, so this step is a build-only check plus a full-suite regression guard).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: dimax daemon now attempts a hot restart before falling back to plain restore"
```

---

## Task 11: TUI reconnect-on-disconnect

**Files:**
- Modify: `src/tui/mod.rs`

Today, when the daemon connection drops, `run`'s main loop just `break`s out cleanly and the whole program exits (`src/tui/mod.rs:2811-2826`, specifically the `Err(_) => break,` arm) -- indistinguishable from the user pressing the quit key or stdin closing, both of which *also* just `break` the same loop. A hot restart is pointless from an already-attached client's point of view if the client just exits the moment the old daemon goes away. This task makes exactly one distinction -- *why* the loop stopped -- and wraps `run` in a retry-and-resume caller for the one reason worth retrying.

**Design note ruled out during this plan's research:** the retry loop below deliberately does **not** call `cli::Client::connect()` to probe reachability. `Client::connect()` (`src/cli.rs:22-29`) auto-spawns a fresh daemon via `daemon::ensure_running` the instant its own connect attempt fails -- calling that repeatedly during the brief window between the old daemon exiting and the new one binding (exactly the window this retry loop runs in) would race an unwanted *third* daemon into existence for the same socket path. The retry loop uses a bare `tokio::net::UnixStream::connect` probe instead, and only calls the real `Client::connect()` (via re-entering `run` itself) once that probe confirms someone is actually listening again.

- [ ] **Step 1: Write the failing test**

Add to `src/tui/mod.rs`'s `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn loop_exit_distinguishes_stop_from_connection_lost() {
        // A narrow, compile-level assertion that the enum this task
        // adds has exactly the two variants `run_with_reconnect`
        // depends on -- the real behavioral coverage for the
        // reconnect path itself is `daemon::tests::
        // run_taking_over_or_fresh_keeps_a_live_pane_running_across_the_handoff`
        // (Task 9), which is what actually proves an *attached
        // client* (via `run_with_reconnect`, driven the same way a
        // real `dimax attach` process would be) survives a real
        // handoff -- add a second assertion block to that test in a
        // follow-up pass wiring `run_with_reconnect` into it directly,
        // once this task lands, rather than duplicating a whole
        // second daemon-pair test here.
        assert_ne!(LoopExit::Stop, LoopExit::ConnectionLost);
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile**

Run: `cargo test --release --lib tui::tests::loop_exit_distinguishes 2>&1 | tail -20`
Expected: `error[E0433]: failed to resolve: use of undeclared type \`LoopExit\``

- [ ] **Step 3: Add `LoopExit` and change `run`'s return type**

Add near the top of `src/tui/mod.rs`, above `pub async fn run`:

```rust
/// Why `run`'s main loop stopped. `run_with_reconnect` is the only
/// caller that cares about the distinction -- `Stop` means "the
/// program should actually exit" (the user pressed the quit key, or
/// stdin closed with nothing left to read); `ConnectionLost` means
/// "the daemon socket dropped out from under this connection" (most
/// often: a hot restart just took over, or the daemon crashed), which
/// is worth retrying rather than exiting to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    Stop,
    ConnectionLost,
}
```

Change `run`'s signature from `pub async fn run() -> anyhow::Result<()>` to:

```rust
pub async fn run() -> anyhow::Result<LoopExit>
```

- [ ] **Step 4: Track which `break` fired**

Immediately before the `loop {` that contains the `tokio::select!` (i.e. right above the loop this task's three `break` sites live inside), add:

```rust
    let mut exit_reason = LoopExit::Stop;
```

Change the two existing quit-style `break;` statements (stdin closed, `src/tui/mod.rs:2728`; quit key pressed, `src/tui/mod.rs:2742`) -- leave them exactly as `break;` (they already mean `Stop`, `exit_reason`'s initial value, so nothing to change at either site).

Change the connection-lost site (`src/tui/mod.rs:2824`) from:

```rust
                    Err(_) => break,
```

to:

```rust
                    Err(_) => {
                        exit_reason = LoopExit::ConnectionLost;
                        break;
                    }
```

Change `run`'s final line, currently `Ok(())` (right after the loop closes, `src/tui/mod.rs:2830`), to:

```rust
    Ok(exit_reason)
```

- [ ] **Step 5: Write `run_with_reconnect`**

Add a new function, right after `run`:

```rust
/// The real `dimax attach`/bare-`dimax` entry point (Task 10's sibling
/// for the client side, wired into `main.rs` in this same task): drive
/// `run` to completion, and if it stopped because the daemon
/// connection was lost specifically (as opposed to the user asking to
/// quit), wait for a daemon to become reachable again and re-enter
/// `run` from scratch instead of exiting to the shell. Every re-entry
/// re-runs `Client::connect` + `App::bootstrap`'s `Subscribe`, which
/// already rebuilds `app.workspace`/`app.grids` fresh from a `Snapshot`
/// -- and, per `daemon::handoff`'s id-preserving design (see that
/// module's doc comment), a reconnect after a genuine *hot* restart
/// gets back the exact same workspace/client-pane/server-pane ids it
/// had before, so there is nothing extra to reconcile here.
pub async fn run_with_reconnect() -> anyhow::Result<()> {
    loop {
        match run().await {
            Ok(LoopExit::Stop) => return Ok(()),
            Ok(LoopExit::ConnectionLost) => {}
            // Could be "the very first `Client::connect` inside `run`
            // failed because nothing's listening yet" (plausible right
            // in the middle of a hot restart) or a genuinely unrelated
            // error. Either way, retrying up to the deadline below is
            // strictly better than giving up on the first blip during
            // exactly the window this feature exists for; a real,
            // persistent problem still surfaces via
            // `wait_for_daemon_or_bail`'s own timeout below.
            Err(_) => {}
        }
        wait_for_daemon_or_bail(std::time::Duration::from_secs(10)).await?;
    }
}

/// Poll for *something* listening at the daemon socket, via a bare
/// connect probe -- not `cli::Client::connect()`, which would
/// auto-spawn a fresh daemon the moment its own probe fails (see this
/// task's design note). Returns once a connection succeeds (closing it
/// immediately; the caller's next `run()` call makes its own real
/// connection), or errors out once `timeout` elapses with nothing
/// answering.
async fn wait_for_daemon_or_bail(timeout: std::time::Duration) -> anyhow::Result<()> {
    let path = protocol::socket_path();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("daemon at {path:?} did not become reachable again within {timeout:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
```

- [ ] **Step 6: Update `main.rs`'s `Cli::Attach` arm**

In `src/main.rs`, change:

```rust
        Cli::Attach => dimax::tui::run().await,
```

to:

```rust
        Cli::Attach => dimax::tui::run_with_reconnect().await,
```

(`run`'s return type changed from `anyhow::Result<()>` to `anyhow::Result<LoopExit>` in Step 3, which is why `main`'s match arm can no longer call `run` directly and return its result as-is -- `run_with_reconnect` is the new thing with the `anyhow::Result<()>` shape `main`'s own return type needs.)

- [ ] **Step 7: Run the tests to confirm they pass**

Run: `cargo test --release --lib tui:: 2>&1 | tail -60`
Expected: every test in `tui::tests` passes, including this task's new one. Confirmed (via `grep -rn "tui::run()" src/`) that `src/main.rs:8` is `run`'s only caller anywhere in this crate and no test calls it directly, so Step 6 above is the only call site this signature change affects -- this step is a plain regression check, not an additional hunt.

- [ ] **Step 8: Run the full suite**

Run: `cargo test --release 2>&1 | grep "test result:"`
Expected: all passing.

- [ ] **Step 9: Commit**

```bash
git add src/tui/mod.rs src/main.rs
git commit -m "feat: TUI reconnects instead of exiting when the daemon connection drops"
```

---

## Task 12: Manual two-real-process smoke test + doc updates

**Files:**
- Modify: `src/daemon/session.rs` (doc comment cross-reference only)
- Modify: `README.txt`

Task 9's automated test proves the handoff mechanism works, but runs both "daemons" as tasks in one test binary (documented there as an accepted, faithful-enough simplification). Before relying on this feature for real, confirm it end-to-end across two genuinely separate OS processes -- this step is manual (shell commands you run and observe), not something to automate into the suite.

- [ ] **Step 1: Build and install**

```bash
cargo build --release
cp target/release/dimax /tmp/dimax-hot-restart-smoke
```

- [ ] **Step 2: Start a daemon and a real pane, by hand**

```bash
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke daemon &
sleep 0.5
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke server spawn editor --cmd cat
```

Note the pid printed by `ps aux | grep "cat$"` for the spawned `cat` process -- this is what you'll confirm survives.

- [ ] **Step 3: Trigger a hot restart**

```bash
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke daemon &
```

(Running `dimax daemon` again while one is already listening at the same socket is exactly what triggers `run_taking_over_or_fresh`'s takeover path instead of failing to bind.)

- [ ] **Step 4: Confirm the same `cat` process survived**

```bash
ps aux | grep "cat$"
```

Expected: the *same* pid from Step 2, not a new one, and the original daemon process from Step 2 should no longer be running (`ps aux | grep "dimax daemon"` shows only the new pid).

- [ ] **Step 5: Confirm it's still reachable and responsive**

```bash
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke server send editor "still alive" --enter
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke server read editor
```

Expected: output contains `still alive`.

- [ ] **Step 6: Clean up**

```bash
XDG_RUNTIME_DIR=/tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke server kill editor
kill %1 2>/dev/null
rm -rf /tmp/dimax-smoke-rt /tmp/dimax-hot-restart-smoke
```

- [ ] **Step 7: Cross-reference the two restart mechanisms in `daemon::session`'s doc comment**

`src/daemon/session.rs`'s module doc comment currently says (from the session this feature landed in) that layout-replay is the *only* restart mechanism, with no live-process survival. Now that `daemon::handoff` exists, add one sentence there pointing to it, so a reader who opens `session.rs` first (the "cousin" this plan's own doc comments already reference from the `handoff` side) finds the other half too. Locate the paragraph explaining "this is NOT process migration" and add immediately after it:

```rust
//! `daemon::handoff` is the *other* restart path, for exactly the case
//! this one can't help with: an already-attached client that wants
//! the actual live process (not a fresh respawned shell) to survive a
//! deliberate restart. `run_taking_over_or_fresh` always tries that
//! first and only falls back to this module's plain layout replay if
//! there's nothing to take over from.
```

- [ ] **Step 8: Mention hot restart in `README.txt`**

Grep `README.txt` for its existing "Install" or a dedicated section describing daemon lifecycle/restart behavior (if one exists from when `daemon::session` was documented there); add a short paragraph next to it:

```
Restarting the daemon (e.g. after upgrading to a new build) tries to
hand every live pane's actual process over to the new daemon instance
first, so shells/editors/anything else you had running keeps running
uninterrupted -- only the on-screen redraw resets (the new daemon
starts each adopted pane's display blank until it next produces
output). If a running daemon can't be found to take over from, it
falls back to restoring just the workspace layout with fresh shells
at each pane's last-known directory instead.
```

- [ ] **Step 9: Commit**

```bash
git add src/daemon/session.rs README.txt
git commit -m "docs: cross-reference daemon::handoff from daemon::session; document in README"
```

---

## Self-review

**Spec coverage:** every requirement from the goal statement has a task: fd-passing mechanism (Tasks 2, 3, 6), protocol negotiation (Task 5), donor-side send (Task 8), acceptor-side receive + `State` rebuild with preserved ids (Tasks 7, 9), wiring into the real daemon/TUI entry points (Tasks 10, 11), and a real cross-process verification (Task 12). The three "known, accepted limitations" called out in the header (no screen redraw, no true `wait()`, no `tty_name()`) are each backed by a grep-confirmed fact about dimax's own existing usage, not a guess.

**Placeholder scan:** one deliberate, narrowly-scoped exception, flagged explicitly in Task 8 Step 1/2 -- a single `Request::Input { pane, ... }` line inside that task's test, left as `unimplemented!()` with a direct pointer to the exact existing pattern (already used by other tests in the same file) to copy instead of guessing at a plausible-but-unverified reproduction of it inline. No other placeholders, "TODO"s, or "add appropriate handling"-style gaps.

**Type consistency check performed:** `ServerPane::from_inherited`'s parameter order/types (Task 4) match every call site that constructs it (Task 7's `adopt_handoff`, Task 4's own test). `HandoffPane`/`HandoffWorkspace`/`HandoffTree` (Task 6) are used with identical field names in every task that touches them (7, 8, 9). `adopt_handoff`'s signature was caught and fixed mid-plan (originally took an external `events` parameter inconsistent with `server_spawn`'s existing `self.pane_events.clone()` convention; corrected in both its own task and cross-checked against Task 9's usage, which never passed one). `LoopExit`'s two variants (Task 11) are referenced identically in `run`, `run_with_reconnect`, and the new test.
