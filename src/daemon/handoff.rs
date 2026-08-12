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

use crate::protocol::{ClientPaneId, ServerPaneId, Size, SplitDir, SplitId, WorkspaceId};

/// Everything about one live server-pane the donor sends alongside its
/// fd, over one `HandoffMessage::Pane` datagram. Unlike
/// `daemon::session::SavedServerPane`, this keeps the *real* id --
/// preserving it is the entire point of a hot restart: an already-
/// attached client's held `ServerPaneId`s stay valid after
/// reconnecting, with no fresh-id remapping needed anywhere.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HandoffWorkspace {
    pub id: WorkspaceId,
    pub number: u8,
    pub name: Option<String>,
    pub tree: Option<HandoffTree>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// One datagram's worth of the handoff exchange. `serde_json`, for
/// consistency with `protocol::framing`'s existing choice, even though
/// the handoff channel doesn't reuse that module's framing itself (see
/// `protocol::Request::BeginHandoff`'s doc comment for why the fd
/// transfer needs its own dedicated socket instead).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// `sendfd`'s tokio-feature impls of `send_with_fd`/`recv_with_fd` are
/// plain (non-async) methods internally built on `try_io` -- they
/// return `WouldBlock` immediately rather than waiting, so a caller
/// must await the socket's own readiness first and retry on
/// `WouldBlock` to get real async behavior out of them.
async fn send_with_fd_async(
    socket: &tokio::net::UnixDatagram,
    bytes: &[u8],
    fds: &[RawFd],
) -> std::io::Result<usize> {
    use sendfd::SendWithFd;
    loop {
        socket.writable().await?;
        match socket.send_with_fd(bytes, fds) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            result => return result,
        }
    }
}

async fn recv_with_fd_async(
    socket: &tokio::net::UnixDatagram,
    bytes: &mut [u8],
    fds: &mut [RawFd],
) -> std::io::Result<(usize, usize)> {
    use sendfd::RecvWithFd;
    loop {
        socket.readable().await?;
        match socket.recv_with_fd(bytes, fds) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            result => return result,
        }
    }
}

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
    let sender = tokio::net::UnixDatagram::unbound()?;
    sender.connect(datagram_path)?;

    let layout = encode(&HandoffMessage::Layout { workspaces });
    send_with_fd_async(&sender, &layout, &[]).await?;

    for (meta, fd) in panes {
        let bytes = encode(&HandoffMessage::Pane { meta });
        send_with_fd_async(&sender, &bytes, &[fd]).await?;
    }

    let done = encode(&HandoffMessage::Done);
    send_with_fd_async(&sender, &done, &[]).await?;
    Ok(())
}

/// What the acceptor gets back from a completed handoff: the layout,
/// plus every `(metadata, inherited fd)` pair -- ready for
/// `State::adopt_handoff` to turn into real `ServerPane`s.
pub struct ReceivedHandoff {
    pub workspaces: Vec<HandoffWorkspace>,
    pub panes: Vec<(HandoffPane, RawFd)>,
}

/// Acceptor side: bind `datagram_path` fresh, then read messages until
/// `Done`. Binds the path itself (rather than taking an already-bound
/// socket) so callers only need to pick a path, matching
/// `send_handoff`'s "you already bound it" contract on the other end.
pub async fn receive_handoff(datagram_path: &std::path::Path) -> anyhow::Result<ReceivedHandoff> {
    if datagram_path.exists() {
        std::fs::remove_file(datagram_path)?;
    }
    let receiver = tokio::net::UnixDatagram::bind(datagram_path)?;

    let mut buf = [0u8; 65536];
    let mut fd_buf = [0 as RawFd; 1];

    let workspaces = loop {
        let (n, _) = recv_with_fd_async(&receiver, &mut buf, &mut fd_buf).await?;
        match decode(&buf[..n])? {
            HandoffMessage::Layout { workspaces } => break workspaces,
            other => anyhow::bail!("expected Layout first, got {other:?}"),
        }
    };

    let mut panes = Vec::new();
    loop {
        let (n, fd_count) = recv_with_fd_async(&receiver, &mut buf, &mut fd_buf).await?;
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

    /// Short filename -- a full-uuid-named path under a long macOS temp
    /// dir can exceed `AF_UNIX`'s `sun_path` limit (~104 bytes); this
    /// stays well under it. Mirrors `tui::tests::app_against_real_daemon`'s
    /// established pattern (pid + a per-process atomic counter, not a
    /// uuid) for the same reason.
    fn short_temp_socket_path(prefix: &str) -> std::path::PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{id}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn send_then_receive_one_pane_message_carries_its_fd() {
        let path = short_temp_socket_path("dmx-ho1");

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
        super::send_with_fd_async(&sender, &bytes, &[read_fd]).await.unwrap();

        let mut buf = [0u8; 4096];
        let mut recv_fds = [0i32; 1];
        let (n, fd_count) = super::recv_with_fd_async(&receiver, &mut buf, &mut recv_fds)
            .await
            .unwrap();
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
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn send_handoff_then_receive_handoff_round_trips_layout_and_panes() {
        let path = short_temp_socket_path("dmx-ho2");

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
        let _ = std::fs::remove_file(&path);
    }
}
