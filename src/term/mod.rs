//! PTY + terminal-emulation glue: owns a spawned process's pseudo-terminal
//! and feeds its output into a `wezterm_term::Terminal` model, exposing a
//! plain [`GridSnapshot`] the daemon can hand to frontends.
//!
//! Contract for callers (the daemon):
//! - [`ServerPane::spawn`] starts the process and a background OS thread
//!   that reads PTY output as it arrives. That thread updates the pane's
//!   internal grid and, on every change, sends a [`ServerPaneEvent`] on
//!   the `events` channel passed in — the daemon does not poll.
//! - All other `ServerPane` methods are synchronous and safe to call from
//!   an async context (they only briefly lock an internal mutex, never
//!   block on I/O) except `write_input`, which does a non-blocking PTY
//!   write.

use crate::protocol::{Cell, ForegroundProcessInfo, GridSnapshot, ServerPaneId, ServerPaneStatus, Size};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtyPair, PtySize};
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use wezterm_term::color::ColorAttribute;
use wezterm_term::{Intensity, Terminal, TerminalConfiguration, TerminalSize, Underline};

/// Pushed by the background reader thread. `events` is an
/// `UnboundedSender` because tokio's unbounded sender can be used from a
/// plain (non-async) OS thread without a runtime handle.
#[derive(Debug, Clone, Copy)]
pub enum ServerPaneEvent {
    Changed(ServerPaneId),
    Died(ServerPaneId),
}

/// Writer handle shared between the `wezterm_term::Terminal` (which uses
/// it to send answerback/response escape sequences back to the child)
/// and [`ServerPane::write_input`] (which forwards raw keystrokes). Both
/// ultimately write to the same PTY master fd; `portable_pty::MasterPty::
/// take_writer` can only be called once, so a single mutex-guarded handle
/// is shared rather than trying to clone the underlying fd.
#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// Minimal `TerminalConfiguration` impl. Every knob besides the color
/// palette keeps the fork's documented defaults (see `config.rs` in the
/// vendored `tattoy-wezterm-term` source) — reasonable for a v1 terminal
/// emulator with no user-configurable settings yet.
#[derive(Debug)]
struct Config;

impl TerminalConfiguration for Config {
    fn color_palette(&self) -> wezterm_term::color::ColorPalette {
        wezterm_term::color::ColorPalette::default()
    }
}

/// Everything shared between `ServerPane` and its background reader
/// thread, guarded by a single `std::sync::Mutex`. Per the module
/// contract, callers only ever hold this lock briefly (no I/O while
/// locked) — the PTY read itself happens in the background thread
/// *outside* the lock, and is only taken to apply the resulting bytes to
/// `terminal`.
struct Inner {
    terminal: Terminal,
    status: ServerPaneStatus,
    size: Size,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: SharedWriter,
}

/// A single PTY-backed process plus its terminal model.
pub struct ServerPane {
    id: ServerPaneId,
    name: Option<String>,
    inner: Arc<Mutex<Inner>>,
}

impl ServerPane {
    /// Spawn `cmd` (or `$SHELL` if `None`) attached to a new PTY sized to
    /// `size`. `id` is chosen by the caller (the daemon) so it can be
    /// referenced before the pane finishes constructing. `events` receives
    /// a `Changed`/`Died` notification from the background reader thread
    /// whenever this pane's displayed content changes or its process
    /// exits.
    pub fn spawn(
        id: ServerPaneId,
        name: Option<String>,
        cmd: Option<String>,
        cwd: Option<String>,
        size: Size,
        events: UnboundedSender<ServerPaneEvent>,
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
            // `sh -c <string>` is the simplest portable way to run an
            // arbitrary shell command string.
            Some(shell_cmd) => {
                let mut b = CommandBuilder::new("/bin/sh");
                b.arg("-c");
                b.arg(shell_cmd);
                b
            }
            // Resolves `$SHELL` (falling back to the password database)
            // and runs it as a login shell — "$SHELL with no args".
            None => CommandBuilder::new_default_prog(),
        };
        // `None` leaves `CommandBuilder`'s own default in place (the
        // spawning process's cwd) -- this only overrides it when a
        // caller actually wants a specific starting directory (the
        // attach menu's per-group "spawn new here" row).
        if let Some(cwd) = cwd {
            builder.cwd(cwd);
        }

        let child = slave.spawn_command(builder)?;
        // Drop our copy of the slave fd once the child has inherited its
        // own: if the parent keeps a slave-side fd open, the child
        // exiting never delivers EOF to `master`'s reader, and this pane
        // would never observe `Died`.
        drop(slave);

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
            "dimux",
            env!("CARGO_PKG_VERSION"),
            Box::new(writer.clone()),
        );
        let mut reader = master.try_clone_reader()?;
        // `try_clone_reader` dups the master's fd (see `unix.rs` in the
        // vendored `portable-pty` source); per POSIX, file *status*
        // flags such as `O_NONBLOCK` are shared across `dup()`'d
        // descriptors sharing one open-file-description, so toggling
        // non-blocking mode on this raw fd affects `reader`'s own reads
        // too, letting the reader thread's drain loop (below) briefly
        // switch to non-blocking reads without a second real fd.
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
                // Block until at least one byte is available -- this is
                // what lets the thread sleep with zero CPU cost when the
                // pane is idle, exactly like the pre-batching version.
                let first = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Any other error (including the EIO some platforms
                    // return once the slave side is fully closed) is
                    // treated the same as a clean EOF: the pane died.
                    Err(_) => break,
                };
                let mut batch = buf[..first].to_vec();

                // See design doc "PTY read batching". A program producing
                // rapid output (measured: ~2,600 reads/sec from `yes`)
                // used to advance the terminal and fire one Changed event
                // per individual `read()`, each acquiring this pane's
                // Inner lock -- at that rate the lock was essentially
                // always held by this thread, starving the daemon's own
                // (occasional, but lock-contending) `snapshot()` calls
                // for the whole duration of a flood.
                //
                // A first attempt drained the kernel's PTY buffer
                // non-blocking immediately after the first read, with no
                // delay. Measured directly against `yes`: it didn't help
                // -- this thread's drain loop runs faster than `yes`
                // refills the buffer, so 91% of "batches" still contained
                // exactly one read, and the Changed rate barely moved
                // (2486/sec vs. 2584/sec baseline). The batch has nothing
                // to find if it looks before the producer has had time to
                // produce more.
                //
                // `BATCH_WINDOW` fixes that: after the first read,
                // deliberately wait before draining, giving a fast
                // producer time to actually queue up several reads' worth
                // of output first. The window is short enough that a
                // single interactive keystroke's echo (the common case,
                // where nothing else arrives during the wait) still feels
                // instant, while a genuine flood gets meaningfully
                // collapsed -- however many reads land during the window
                // become one `advance_bytes` call and one event, cutting
                // the lock's acquisition *rate* (and the daemon's
                // broadcast rate) to roughly `1 / BATCH_WINDOW` under
                // sustained output instead of the raw PTY read frequency.
                const BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
                std::thread::sleep(BATCH_WINDOW);

                if let Some(fd) = raw_fd {
                    set_nonblocking(fd, true);
                    const MAX_BATCH_READS: usize = 64;
                    for _ in 0..MAX_BATCH_READS {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => batch.extend_from_slice(&buf[..n]),
                            // EAGAIN/EWOULDBLOCK: nothing more buffered
                            // right now -- stop draining, this is the
                            // normal/expected end of a batch, not an
                            // error.
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
                // Ignore send errors: if nobody is listening any more we
                // still want to keep draining the PTY so the child never
                // blocks on a full output buffer.
                let _ = events.send(ServerPaneEvent::Changed(id));
            }
            {
                let mut guard = thread_inner.lock().unwrap();
                guard.status = ServerPaneStatus::Dead;
            }
            let _ = events.send(ServerPaneEvent::Died(id));
        });

        Ok(Self { id, name, inner })
    }

    pub fn id(&self) -> ServerPaneId {
        self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    /// `Running` while the child process is alive, `Dead` after it exits
    /// (the grid keeps its last contents either way).
    pub fn status(&self) -> ServerPaneStatus {
        self.inner.lock().unwrap().status
    }

    pub fn size(&self) -> Size {
        self.inner.lock().unwrap().size
    }

    /// Rows of actual history available to scroll back into (excludes
    /// the live on-screen rows) -- the upper bound
    /// `daemon::state::State::scroll_server_pane` clamps against. NOT
    /// the same number as `wezterm_term::Screen::scrollback_rows()`,
    /// despite the matching name and this being a thin wrapper around
    /// it: that method returns the *total* buffered line count (visible
    /// rows + history -- a fresh pane with zero history still reports
    /// `physical_rows`, not 0), so `physical_rows` must be subtracted to
    /// get the actual scrollable depth.
    pub fn scrollback_rows(&self) -> usize {
        let guard = self.inner.lock().unwrap();
        let screen = guard.terminal.screen();
        screen.scrollback_rows().saturating_sub(screen.physical_rows)
    }

    /// A live OS-level snapshot of this PTY's foreground process — see
    /// design doc "Attach menu identification columns": queried fresh on
    /// every call rather than tracked/cached, since callers (the attach
    /// menu, `dimux server ls`) already re-fetch on their own cadence.
    ///
    /// `MasterPty::process_group_leader` (already provided by
    /// `portable-pty`, no unsafe code needed here) gives the PID of
    /// whichever process is currently in the foreground of this PTY —
    /// e.g. `vim` if you ran it inside the pane's shell, not the shell
    /// itself. That PID is then looked up via `sysinfo` for its command
    /// name and working directory.
    ///
    /// Returns `None` if there's no foreground process to query (a
    /// `Dead` pane, or the OS call itself failing) or the PID can no
    /// longer be found in the process table (a race between the lookup
    /// and the process exiting — treated the same as "nothing to show"
    /// rather than an error, since this is best-effort diagnostic info).
    pub fn foreground_info(&self) -> Option<ForegroundProcessInfo> {
        let guard = self.inner.lock().unwrap();
        let pid = guard.master.process_group_leader()?;
        drop(guard); // release before the (comparatively slow) OS query below

        let mut system = sysinfo::System::new();
        let sysinfo_pid = sysinfo::Pid::from_u32(pid as u32);
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]),
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cwd(sysinfo::UpdateKind::Always)
                .with_cmd(sysinfo::UpdateKind::Always),
        );
        let process = system.process(sysinfo_pid)?;

        Some(ForegroundProcessInfo {
            process_name: process.name().to_string_lossy().into_owned(),
            cwd: process.cwd().map(|p| p.display().to_string()),
        })
    }

    /// `offset` shifts the returned window back from the live tail by
    /// that many rows (0 = today's exact live-view behavior, preserved
    /// unchanged for every caller not yet updated for scrolling).
    /// Clamping `offset` to a sane range is the *caller*'s
    /// responsibility (`daemon::state::State::scroll_server_pane`
    /// clamps against `scrollback_rows()` before this is ever called
    /// with a value from user input) -- this method trusts its input.
    pub fn snapshot(&self, offset: usize) -> GridSnapshot {
        let guard = self.inner.lock().unwrap();
        let screen = guard.terminal.screen();
        let rows = screen.physical_rows;
        let cols = screen.physical_cols;
        let start = -(offset as i32);
        let end = start + rows as i32;
        let phys_range = screen.scrollback_or_visible_range(&(start..end));
        let lines = screen.lines_in_phys_range(phys_range);

        let mut grid_lines: Vec<Vec<Cell>> = lines.iter().map(|l| line_to_cells(l, cols)).collect();
        // Pad with blank rows if the screen somehow reports fewer
        // physical lines than its declared height, so `lines.len()`
        // always matches `size.rows` for callers.
        while grid_lines.len() < rows {
            grid_lines.push(vec![blank_cell(); cols]);
        }

        let cursor = guard.terminal.cursor_pos();
        GridSnapshot {
            server_pane: self.id,
            size: guard.size,
            // `(col, row)`, matching `CursorPosition`'s own `(x, y)`.
            cursor: (
                cursor.x.min(u16::MAX as usize) as u16,
                cursor.y.max(0).min(u16::MAX as i64) as u16,
            ),
            lines: grid_lines,
            scroll_offset: offset,
        }
    }

    /// Render the current live screen (`snapshot(0)`) as plain text for
    /// scripting callers (`Request::ServerRead`) that want the characters
    /// on screen, not a styled `GridSnapshot` -- one line per row, each
    /// right-trimmed of the padding `blank_cell` fills unused columns
    /// with, then trailing all-blank rows dropped so a mostly-empty pane
    /// doesn't read back as a wall of blank lines.
    pub fn snapshot_text(&self) -> String {
        let snapshot = self.snapshot(0);
        let mut lines: Vec<String> = snapshot
            .lines
            .iter()
            .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>().trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Write raw bytes (keystrokes) to the child process's stdin via the
    /// PTY master.
    pub fn write_input(&self, bytes: &[u8]) -> anyhow::Result<()> {
        // Clone the (cheap, Arc-backed) writer handle and release the
        // `Inner` lock before doing any I/O, per the module contract.
        let mut writer = self.inner.lock().unwrap().writer.clone();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize both the OS-level PTY and the `wezterm_term::Terminal`
    /// model. Called by the daemon after recomputing the
    /// smallest-viewer-wins size for this pane.
    pub fn resize(&self, size: Size) -> anyhow::Result<()> {
        let mut guard = self.inner.lock().unwrap();
        guard.master.resize(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        guard.terminal.resize(TerminalSize {
            rows: size.rows as usize,
            cols: size.cols as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });
        guard.size = size;
        Ok(())
    }

    /// Terminate the child process. `status()` will report `Dead`
    /// afterward; `snapshot()` continues to return the last grid.
    pub fn kill(&mut self) -> anyhow::Result<()> {
        let mut guard = self.inner.lock().unwrap();
        guard.child.kill()?;
        // Set eagerly rather than waiting on the background reader
        // thread to observe PTY EOF, so the postcondition documented
        // above holds as soon as `kill()` returns.
        guard.status = ServerPaneStatus::Dead;
        Ok(())
    }
}

/// Toggle `O_NONBLOCK` on a raw fd via `fcntl`. Used by the reader
/// thread's read-batching loop (see `ServerPane::spawn`'s doc comment on
/// the drain loop) to briefly drain whatever the kernel already has
/// buffered without blocking, then switch back to blocking mode so the
/// thread properly sleeps (zero CPU) between output bursts rather than
/// busy-polling. Failures are ignored -- worst case a `set(true)` that
/// silently didn't take effect just means the drain loop's `read` blocks
/// once instead of returning `WouldBlock`, which the loop already treats
/// as "stop draining" via its `Err(_) => break` fallback, so there's no
/// correctness issue, only a missed optimization on that one iteration.
fn set_nonblocking(fd: RawFd, nonblocking: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 {
            return;
        }
        let new_flags =
            if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        libc::fcntl(fd, libc::F_SETFL, new_flags);
    }
}

/// Convert a `wezterm_term` screen line into the wire-level `Cell` row
/// used by [`GridSnapshot`], padding out to `cols` (lines may be shorter
/// than the physical width once trailing blank cells are pruned from
/// storage). Kept as a free function so it's testable in isolation
/// against constructed `wezterm_term` state without needing a live PTY.
fn line_to_cells(line: &wezterm_term::Line, cols: usize) -> Vec<Cell> {
    let mut row = vec![blank_cell(); cols];
    for cell_ref in line.visible_cells() {
        let idx = cell_ref.cell_index();
        if idx >= cols {
            break;
        }
        let attrs = cell_ref.attrs();
        row[idx] = Cell {
            text: cell_ref.str().to_string(),
            fg: color_attr_to_rgb(attrs.foreground()),
            bg: color_attr_to_rgb(attrs.background()),
            bold: matches!(attrs.intensity(), Intensity::Bold),
            italic: attrs.italic(),
            underline: !matches!(attrs.underline(), Underline::None),
            reverse: attrs.reverse(),
        };
        // A wide cell's trailing half stays a blank placeholder in `row`
        // (`visible_cells` doesn't yield a separate entry for it), which
        // is exactly the representation we want on the wire.
    }
    row
}

fn blank_cell() -> Cell {
    Cell {
        text: " ".to_string(),
        fg: None,
        bg: None,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    }
}

/// Standard xterm 16-color ANSI palette, used only as a v1 approximation
/// for `ColorAttribute::PaletteIndex(0..16)`. Anything outside that range
/// (256-color palette, or a palette entry the terminal app never set)
/// resolves to `None` rather than growing a full 256-entry table here —
/// an acceptable simplification for v1 per the module contract.
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn color_attr_to_rgb(attr: ColorAttribute) -> Option<(u8, u8, u8)> {
    match attr {
        ColorAttribute::Default => None,
        ColorAttribute::TrueColorWithDefaultFallback(srgb)
        | ColorAttribute::TrueColorWithPaletteFallback(srgb, _) => {
            let (r, g, b, _a) = srgb.to_srgb_u8();
            Some((r, g, b))
        }
        ColorAttribute::PaletteIndex(idx) => ANSI16.get(idx as usize).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;
    use uuid::Uuid;

    /// Concatenate every row's cell text into one string, newline
    /// separated, for simple `contains()` assertions in tests.
    fn snapshot_text(pane: &ServerPane) -> String {
        pane.snapshot(0)
            .lines
            .iter()
            .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Block on `rx` (via `blocking_recv`, which tokio explicitly
    /// supports outside an async context) until `pred` is true or the
    /// channel closes because the background reader thread exited.
    fn wait_until<F: FnMut() -> bool>(rx: &mut UnboundedReceiver<ServerPaneEvent>, mut pred: F) -> bool {
        if pred() {
            return true;
        }
        while rx.blocking_recv().is_some() {
            if pred() {
                return true;
            }
        }
        pred()
    }

    #[test]
    fn spawn_prints_output_and_dies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::spawn(
            id,
            None,
            Some("printf hello-dimux".to_string()),
            None,
            Size { rows: 24, cols: 80 },
            tx,
        )
        .unwrap();

        let found = wait_until(&mut rx, || snapshot_text(&pane).contains("hello-dimux"));
        assert!(found, "expected snapshot to contain \"hello-dimux\"");

        // A `printf` with no further work should exit near-instantly;
        // poll for `Dead` with a generous timeout rather than assume a
        // single event ordering.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while pane.status() != ServerPaneStatus::Dead && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(pane.status(), ServerPaneStatus::Dead);
    }

    #[test]
    fn write_input_is_echoed_then_kill_marks_dead() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let mut pane = ServerPane::spawn(
            id,
            None,
            Some("cat".to_string()),
            None,
            Size { rows: 24, cols: 80 },
            tx,
        )
        .unwrap();

        pane.write_input(b"ping\n").unwrap();

        let found = wait_until(&mut rx, || snapshot_text(&pane).contains("ping"));
        assert!(found, "expected snapshot to contain \"ping\" after write_input");

        pane.kill().unwrap();
        assert_eq!(pane.status(), ServerPaneStatus::Dead);
    }

    #[test]
    fn snapshot_text_trims_trailing_blank_rows_and_padding() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::spawn(
            id,
            None,
            Some("printf hi".to_string()),
            None,
            Size { rows: 24, cols: 80 },
            tx,
        )
        .unwrap();

        let found = wait_until(&mut rx, || pane.snapshot_text().contains("hi"));
        assert!(found, "expected snapshot_text to contain \"hi\"");

        let text = pane.snapshot_text();
        assert_eq!(text, "hi", "should be exactly the one non-blank row, right-trimmed, with no trailing blank rows");
    }

    /// Regression test for the "dimux hangs often" investigation: a
    /// rapidly-flooding pane (`yes`) must not fire a `Changed` event per
    /// individual PTY read. Measured before this test existed: an
    /// unbatched reader thread produced ~2,584 events/sec for `yes`,
    /// each one briefly locking this pane's `Inner` -- enough to starve
    /// the daemon's own occasional, lock-contending `snapshot()` calls
    /// for the whole duration of a flood. With `BATCH_WINDOW` batching,
    /// the same workload produces roughly `1000ms / 8ms ≈ 125` events/sec
    /// at most. This test asserts well under that, with a generous
    /// margin for scheduler variance across machines/CI -- it exists to
    /// catch a gross regression back to per-read events, not to pin an
    /// exact number.
    #[test]
    fn flooding_pane_batches_changed_events_instead_of_firing_per_read() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::spawn(id, None, Some("yes".to_string()), None, Size { rows: 24, cols: 80 }, tx)
            .unwrap();

        // Let `yes` actually get its output flowing before measuring, so
        // the count isn't diluted by process startup time.
        std::thread::sleep(std::time::Duration::from_millis(100));
        while rx.try_recv().is_ok() {}

        let mut count = 0u64;
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_secs(1);
        while start.elapsed() < window {
            if rx.try_recv().is_ok() {
                count += 1;
            }
        }

        assert!(
            count < 500,
            "expected well under 500 Changed events/sec from a flooding pane with batching \
             active, got {count} -- this looks like a regression back to firing one event per \
             individual PTY read (measured baseline without batching: ~2,584/sec)"
        );

        let mut pane = pane;
        pane.kill().unwrap();
    }

    /// Batching must not drop or reorder output -- several distinct
    /// writes spaced across the `BATCH_WINDOW` boundary should all still
    /// land, in order, in the final grid.
    #[test]
    fn batching_preserves_all_output_across_multiple_writes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), None, Size { rows: 24, cols: 80 }, tx).unwrap();

        for line in ["alpha", "bravo", "charlie"] {
            pane.write_input(format!("{line}\n").as_bytes()).unwrap();
            // Sleep past one batch window between writes so each one is
            // genuinely a separate read-and-batch cycle, not accidentally
            // coalesced into a single one by test timing.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let found =
            wait_until(&mut rx, || snapshot_text(&pane).contains("alpha")
                && snapshot_text(&pane).contains("bravo")
                && snapshot_text(&pane).contains("charlie"));
        assert!(found, "expected all three writes to appear in the snapshot");

        let text = snapshot_text(&pane);
        let alpha_pos = text.find("alpha").unwrap();
        let bravo_pos = text.find("bravo").unwrap();
        let charlie_pos = text.find("charlie").unwrap();
        assert!(
            alpha_pos < bravo_pos && bravo_pos < charlie_pos,
            "expected alpha/bravo/charlie to appear in write order, got: {text:?}"
        );
    }

    #[test]
    fn resize_updates_size() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::spawn(
            id,
            None,
            Some("cat".to_string()),
            None,
            Size { rows: 24, cols: 80 },
            tx,
        )
        .unwrap();

        assert_eq!(pane.size(), Size { rows: 24, cols: 80 });
        pane.resize(Size { rows: 10, cols: 40 }).unwrap();
        assert_eq!(pane.size(), Size { rows: 10, cols: 40 });
    }

    #[test]
    fn spawn_with_cwd_starts_the_shell_in_that_directory() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        // `/tmp` resolves to `/private/tmp` on macOS -- `pwd`'s actual
        // printed output is the source of truth here, not the path
        // string passed in, so this asserts against whatever `pwd`
        // prints rather than the literal `"/tmp"`.
        let pane = ServerPane::spawn(
            id,
            None,
            Some("pwd".to_string()),
            Some("/tmp".to_string()),
            Size { rows: 24, cols: 80 },
            tx,
        )
        .unwrap();

        let found = wait_until(&mut rx, || !snapshot_text(&pane).trim().is_empty());
        assert!(found, "expected pwd to print something");
        let text = snapshot_text(&pane);
        assert!(
            text.contains("tmp"),
            "expected pwd's output to reflect the /tmp cwd it was spawned with, got: {text:?}"
        );
    }

    #[test]
    fn scrollback_rows_is_zero_for_a_fresh_pane() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), None, Size { rows: 24, cols: 80 }, tx).unwrap();
        assert_eq!(pane.scrollback_rows(), 0);
    }

    #[test]
    fn snapshot_at_nonzero_offset_shows_scrolled_off_content() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        // A small 5-row pane makes it easy to scroll content off the
        // top with a modest number of printed lines.
        let pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), None, Size { rows: 5, cols: 80 }, tx).unwrap();

        // Write enough lines to scroll "first-line" off the top of a
        // 5-row screen and into scrollback.
        for i in 0..20 {
            pane.write_input(format!("line-{i}\n").as_bytes()).unwrap();
        }
        let found = wait_until(&mut rx, || {
            pane.snapshot(0)
                .lines
                .iter()
                .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
                .contains("line-19")
        });
        assert!(found, "expected the live view to eventually show the last written line");

        assert!(pane.scrollback_rows() > 0, "expected some scrollback to have accumulated");

        // At offset 0 (live), the most recent content should be
        // visible; at a nonzero offset, it should NOT be (we've
        // scrolled back away from it).
        let live_text: String = pane
            .snapshot(0)
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(live_text.contains("line-19"));

        let scrolled = pane.snapshot(pane.scrollback_rows());
        assert_eq!(scrolled.scroll_offset, pane.scrollback_rows());
        let scrolled_text: String = scrolled
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(
            !scrolled_text.contains("line-19"),
            "scrolled all the way back should show older content, not the latest line"
        );
    }

    #[test]
    fn foreground_info_reports_the_running_shell_command() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane = ServerPane::spawn(id, None, Some("cat".to_string()), None, Size { rows: 24, cols: 80 }, tx)
            .unwrap();

        // Wait for the shell to actually exec `cat` (there's a brief
        // window right after spawn where the foreground process is still
        // /bin/sh, before it execs into cat) by polling until the name
        // resolves to something other than the launching shell, or until
        // a generous timeout. wait_until only fires on Changed/Died
        // events, which cat produces none of while idle, so poll
        // directly with a short sleep instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut info = pane.foreground_info();
        while info.as_ref().is_none_or(|i| i.process_name != "cat") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            info = pane.foreground_info();
        }
        let info = info.expect("expected a foreground process to be reported for a live pane");
        assert_eq!(info.process_name, "cat");
        // cwd is best-effort and platform-dependent; just confirm it's
        // populated with *something* plausible rather than asserting an
        // exact value this test can't portably know.
        assert!(info.cwd.is_some(), "expected a cwd to be reported");

        // Drain the (unused in this test) events channel's sender so it
        // doesn't linger -- matches the pattern of other tests that
        // construct `rx` even when not polling it for Changed/Died.
        let _ = rx.try_recv();
    }

    #[test]
    fn foreground_info_is_none_after_kill() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let mut pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), None, Size { rows: 24, cols: 80 }, tx).unwrap();
        pane.kill().unwrap();
        // The process group leader is gone once killed; there's nothing
        // left to query.
        assert!(pane.foreground_info().is_none());
    }
}
