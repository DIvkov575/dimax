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
//! reimplements every trait method the same way -- the ones dimax
//! calls for real, plus `get_size`/`tty_name` the trait still requires.

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

    /// Deliberately simpler than upstream `UnixMasterPty::take_writer`:
    /// that impl (a) guards against being called more than once (bails
    /// rather than letting two writers race to send EOT on drop), and
    /// (b) sends an EOT byte on drop so a foreground shell sees a clean
    /// EOF. Neither is replicated here -- dimax only ever calls this
    /// once per pane (confirmed by grep) and tears panes down via
    /// `Child::kill()`, not writer-drop, so both gaps are currently
    /// inert. Revisit if a future caller starts relying on either.
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

    /// No timeout: if `self.pid` is never observed dead (e.g. reused by
    /// an unrelated long-lived process before this ever gets called --
    /// currently impossible, since nothing in dimax calls `wait()` on
    /// an adopted pane), this blocks the calling thread forever.
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

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{MasterPty, PtySize, SlavePty, native_pty_system};
    use std::io::{Read, Write};

    /// A real pty pair, keeping the *original* master (and, crucially,
    /// the slave -- see below) alive while a fresh `dup()` of the
    /// master's fd is wrapped in `InheritedMasterPty` -- proving the
    /// adopted wrapper behaves identically to a portable-pty-owned one
    /// for every operation dimax actually performs on a `MasterPty`.
    ///
    /// The slave must be returned and kept alive by the caller too: on
    /// macOS/BSD, once the last open reference to the slave side closes,
    /// writes to the master fail with `EIO` even though the master fd
    /// itself is still open (`PtyPair` deliberately lists `slave` before
    /// `master` so it drops first in the common case -- see the doc
    /// comment on `portable_pty::PtyPair` -- so a caller that discards
    /// the returned slave immediately would hit exactly this).
    fn real_pty_pair() -> (Box<dyn MasterPty + Send>, Box<dyn SlavePty + Send>, i32) {
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
        (pair.master, pair.slave, dup_fd)
    }

    #[test]
    fn inherited_master_pty_can_write_and_read_through_the_real_pty() {
        let (master, _slave, dup_fd) = real_pty_pair();
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
        let (master, _slave, dup_fd) = real_pty_pair();
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

        // Verification note (deviates from a plain `kill(pid, 0)`
        // liveness poll -- see below for why): this test process is
        // still the *real* kernel-level parent of `pid` -- `mem::forget`
        // only drops the Rust-side `std::process::Child` handle, it
        // doesn't reparent the process to init. So once SIGKILL lands,
        // `pid` becomes a zombie only *we* can reap, and a zombie still
        // answers `kill(pid, 0)` with success (confirmed by manual
        // repro on this machine: `ps` shows `STAT Z <defunct>` yet the
        // liveness probe keeps returning 0 indefinitely) -- that
        // liveness-poll version of this test fails 100% of the time
        // here, not flakily, for a reason that has nothing to do with
        // whether `kill()` worked (it does: the signal is delivered and
        // the process is in fact dead, just unreaped). In the real
        // scenario this wraps, the adopted process's actual parent is
        // init, which reaps its children as they die, so no such
        // permanent zombie is possible there and the liveness probe
        // `try_wait`/`wait` use is accurate in production. Here, we
        // instead reap the zombie ourselves via `waitpid` and assert it
        // died specifically from the `SIGKILL` we just sent -- strictly
        // stronger proof that `kill()` did its job than any liveness
        // poll, and not dependent on reaping races.
        let mut status: i32 = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(waited, pid, "waitpid should reap the process we just killed");
        assert!(
            libc::WIFSIGNALED(status),
            "process should have died from a signal, raw status = {status:#x}"
        );
        assert_eq!(
            libc::WTERMSIG(status),
            libc::SIGKILL,
            "process should have died specifically from our SIGKILL"
        );
    }
}
