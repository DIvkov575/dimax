//! `MasterPty`/`Child` implementations for a PTY master fd and child pid
//! that *already existed* before this process started -- inherited
//! across an `execve` during a hot reload (see `daemon::mod`'s "Hot
//! reload" module doc), rather than freshly created by
//! `portable_pty::native_pty_system().openpty()` + `spawn_command()`.
//!
//! `portable_pty`'s own Unix implementation (`UnixMasterPty` in its
//! vendored `unix.rs`) is a thin wrapper around exactly the same raw fd
//! plus a handful of ioctls -- nothing about it depends on how the fd
//! was obtained. It's not `pub`, so it can't be reused directly; this
//! module reimplements the same primitives (`TIOCSWINSZ`/`TIOCGWINSZ`,
//! `tcgetpgrp`) against an fd this process didn't create itself.
//!
//! `Child`/`ChildKiller` only need to support `kill()` and
//! `process_id()` here -- see `ServerPane`'s module doc: death detection
//! is driven entirely by the background reader thread's `read()`
//! returning EOF, never by `Child::wait`/`try_wait`, so those two are
//! best-effort only. `waitpid` on an inherited pid still works correctly
//! even post-exec: exec replaces a process's *code*, not its identity or
//! its parent/child relationships, so a shell that was a child of the
//! pre-reload daemon is still this same (now newly-exec'd) process's
//! child afterward.

use anyhow::Error as PtyError;
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::PathBuf;

/// Duplicate `fd` with `FD_CLOEXEC` set on the copy, matching the
/// `filedescriptor` crate's own `F_DUPFD_CLOEXEC`-based `try_clone` --
/// the copies `try_clone_reader`/`take_writer` hand out must not leak
/// into a *future* re-exec the way the one canonical fd per pane
/// (cleared of `FD_CLOEXEC` right before `execve`, see
/// `daemon::mod::prepare_reload`) deliberately does.
fn dup_cloexec(fd: RawFd) -> std::io::Result<RawFd> {
    let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duped == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(duped)
}

pub struct InheritedMasterPty {
    fd: RawFd,
    took_writer: std::cell::Cell<bool>,
}

impl InheritedMasterPty {
    /// # Safety
    /// `fd` must be an open, valid PTY master file descriptor that this
    /// call takes ownership of (closed on drop, via the eventual
    /// `File`/fd cleanup once nothing references it -- there is
    /// deliberately no `Drop` impl here beyond what dropping the raw
    /// value implies, since `master`'s own fd is the *one* the whole
    /// pane's lifetime hangs off of, exactly like `UnixMasterPty`'s).
    pub unsafe fn new(fd: RawFd) -> Self {
        Self {
            fd,
            took_writer: std::cell::Cell::new(false),
        }
    }
}

impl Drop for InheritedMasterPty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl MasterPty for InheritedMasterPty {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        if unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ as _, &ws as *const _) } != 0 {
            anyhow::bail!(
                "failed to ioctl(TIOCSWINSZ): {:?}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, PtyError> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd, libc::TIOCGWINSZ as _, &mut ws as *mut _) } != 0 {
            anyhow::bail!(
                "failed to ioctl(TIOCGWINSZ): {:?}",
                std::io::Error::last_os_error()
            );
        }
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, PtyError> {
        let duped = dup_cloexec(self.fd)?;
        Ok(Box::new(unsafe { File::from_raw_fd(duped) }))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, PtyError> {
        if self.took_writer.replace(true) {
            anyhow::bail!("cannot take writer more than once");
        }
        let duped = dup_cloexec(self.fd)?;
        Ok(Box::new(unsafe { File::from_raw_fd(duped) }))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd)
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// See module doc: only `kill`/`process_id` are load-bearing.
/// `try_wait`/`wait` are best-effort `waitpid` calls, never actually
/// called by this codebase (death detection is EOF-driven).
#[derive(Debug, Clone, Copy)]
pub struct InheritedChild {
    pid: libc::pid_t,
}

impl InheritedChild {
    pub fn new(pid: u32) -> Self {
        Self {
            pid: pid as libc::pid_t,
        }
    }
}

impl ChildKiller for InheritedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        if unsafe { libc::kill(self.pid, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(*self)
    }
}

impl Child for InheritedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if rc == 0 {
            return Ok(None);
        }
        if rc == self.pid {
            return Ok(Some(ExitStatus::with_exit_code(exit_code_of(status))));
        }
        // `rc == -1`: most commonly ECHILD (already reaped by something
        // else, or never actually our child) -- treat as "can't tell",
        // not an error worth surfacing given nothing calls this.
        Ok(None)
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(self.pid, &mut status, 0) };
        if rc == self.pid {
            return Ok(ExitStatus::with_exit_code(exit_code_of(status)));
        }
        Err(std::io::Error::last_os_error())
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid as u32)
    }
}

fn exit_code_of(status: libc::c_int) -> u32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else {
        // Killed by a signal -- no POSIX exit code; 128+signal is the
        // conventional shell encoding, close enough for a value nothing
        // in this codebase actually inspects.
        128 + libc::WTERMSIG(status) as u32
    }
}
