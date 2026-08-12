//! Plain timing numbers for the 5 focus-unaware hot paths found while
//! scoping a "prioritize the focused pane" optimization: snapshot cost,
//! wire serialization, daemon broadcast fan-out, TUI render, and global
//! lock contention. Loop N times, print avg time -- no statistics
//! framework, no warmup/outlier detection. Run with:
//!   cargo run --release --bin bench_hot_paths

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dimax::daemon::state::State;
use dimax::protocol::{Request, Response, ServerMessage, Size, SplitDir};
use dimax::term::ServerPane;
use dimax::tui::render;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

fn avg_us(total: Duration, iters: u32) -> f64 {
    total.as_secs_f64() * 1_000_000.0 / iters as f64
}

fn spawn_filled_pane(rows: u16, cols: u16) -> ServerPane {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pane = ServerPane::spawn(
        uuid::Uuid::new_v4(),
        None,
        Some("cat".to_string()),
        None,
        Size { rows, cols },
        tx,
        None,
        "bench".to_string(),
    )
    .expect("spawn cat for benchmark fixture");

    // One color switch per line (not per character): a per-character
    // escape code balloons a 200+-col line past the PTY's canonical-mode
    // line buffer limit (macOS `TTYHOG`, ~1024 bytes) with no newline in
    // sight, which blocks the write forever -- exactly the hang this
    // comment replaces. Still gives every row a distinct, non-default
    // fg color to exercise `color_attr_to_rgb`'s truecolor path.
    let mut payload = String::new();
    for row in 0..rows {
        let color = 16 + (row as u32 * 7) % 216;
        payload.push_str(&format!("\x1b[38;5;{color}m"));
        payload.push_str(&"X".repeat(cols as usize));
        payload.push_str("\x1b[0m\n");
    }
    pane.write_input(payload.as_bytes())
        .expect("write fixture payload");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = pane.snapshot(0);
        if snap
            .lines
            .last()
            .is_some_and(|row| row.iter().any(|c| c.text != " "))
        {
            break;
        }
        if Instant::now() > deadline {
            panic!("fixture pane never filled its grid within 5s");
        }
        let _ = rx.blocking_recv();
    }
    pane
}

fn bench_snapshot() {
    println!("\n== 1. snapshot cost (ServerPane::snapshot) ==");
    for &(rows, cols) in &[(24u16, 80u16), (50, 200), (100, 300)] {
        let mut pane = spawn_filled_pane(rows, cols);
        let iters = 200;
        let start = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(pane.snapshot(0));
        }
        println!("  {rows}x{cols}: {:.1} us/op", avg_us(start.elapsed(), iters));
        // `ServerPane` has no `Drop` impl (by design -- the daemon owns
        // that lifecycle via `State::server_kill`); a benchmark fixture
        // has no daemon, so it must kill its own `cat` process and
        // reader thread explicitly, or every fixture leaks both for the
        // rest of the run -- accumulating leaked processes/threads is
        // exactly what made earlier runs of this binary progressively
        // slow down and eventually stall.
        let _ = pane.kill();
    }
}

fn bench_serialize() {
    println!("\n== 2. GridDelta JSON serialize (serde_json::to_vec) ==");
    for &(rows, cols) in &[(24u16, 80u16), (50, 200), (100, 300)] {
        let mut pane = spawn_filled_pane(rows, cols);
        let snapshot = pane.snapshot(0);
        let event = ServerMessage::Event(dimax::protocol::Event::GridDelta { snapshot });
        let iters = 200;
        let start = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(serde_json::to_vec(&event).unwrap());
        }
        println!("  {rows}x{cols}: {:.1} us/op", avg_us(start.elapsed(), iters));
        let _ = pane.kill();
    }
}

fn bench_broadcast_fanout() {
    println!("\n== 3. broadcast fan-out (per-subscriber snapshot cost) ==");
    for &subscriber_count in &[1u64, 4, 16] {
        let mut state = State::new();
        let server_pane = state
            .server_spawn(None, Some("cat".to_string()), None, None)
            .unwrap()
            .id;
        for i in 0..subscriber_count {
            // A name, not a bare number: `resolve_or_create_workspace`
            // caps numeric targets at 1-9 (real workspace-number chords
            // only go that far), but this loop runs past 9 subscribers.
            let ws = state
                .resolve_or_create_workspace(&format!("bench-ws-{i}"))
                .unwrap();
            state.client_spawn(ws, None, None, Some(server_pane)).unwrap();
            state.subscribe(i, ws);
        }
        let iters = 200;
        let start = Instant::now();
        for _ in 0..iters {
            let subs = state.subscribers_for_server_pane(server_pane);
            let pane = state.server_pane(server_pane).unwrap();
            for _ in &subs {
                std::hint::black_box(pane.snapshot(0));
            }
        }
        println!(
            "  {subscriber_count} subscriber(s): {:.1} us/op",
            avg_us(start.elapsed(), iters)
        );
        let _ = state.server_kill(&server_pane.to_string());
    }
}

/// A `GridSnapshot` filled with varied, non-blank content, built
/// directly with no PTY/process involved -- `render::draw` (what
/// `bench_render` measures) only ever reads a pre-fetched `grids` map,
/// never a live `ServerPane`, so spawning a real `cat` just to snapshot
/// it once was pure overhead this benchmark never needed. That overhead
/// is also what made `bench_render`'s multi-leaf cases (each needing
/// its own fixture pane) compound atop benchmarks 1-3's already-spawned
/// panes until spawning stalled outright -- this removes the
/// compounding, not just its symptom.
fn synthetic_grid(server_pane: uuid::Uuid, rows: u16, cols: u16) -> dimax::protocol::GridSnapshot {
    use dimax::protocol::Cell;
    let lines = (0..rows)
        .map(|row| {
            let (r, g, b) = ((row * 7) as u8, (row * 13) as u8, (row * 23) as u8);
            (0..cols)
                .map(|_| Cell {
                    text: "X".to_string(),
                    fg: Some((r, g, b)),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                })
                .collect()
        })
        .collect();
    dimax::protocol::GridSnapshot {
        server_pane,
        size: Size { rows, cols },
        cursor: (0, 0),
        lines,
        scroll_offset: 0,
    }
}

/// Build a `leaf_count`-leaf split workspace for `bench_render`,
/// entirely in-memory (no `State`, no real server-panes) -- a
/// left-leaning chain of vertical splits, the same shape a real user's
/// split-heavy workspace would have.
fn build_split_workspace(
    leaf_count: usize,
    rows: u16,
    cols: u16,
) -> (
    dimax::protocol::WorkspaceInfo,
    HashMap<uuid::Uuid, dimax::protocol::GridSnapshot>,
) {
    use dimax::protocol::{ClientPane, SplitTree, WorkspaceInfo};

    let mut grids = HashMap::new();
    let mut tree: Option<SplitTree> = None;
    for i in 0..leaf_count {
        let server_pane = uuid::Uuid::new_v4();
        grids.insert(server_pane, synthetic_grid(server_pane, rows, cols));
        let leaf = SplitTree::Leaf(ClientPane {
            id: uuid::Uuid::new_v4(),
            name: None,
            tabs: vec![server_pane],
            active_tab: 0,
            short_id: format!("{i:02}"),
        });
        tree = Some(match tree {
            None => leaf,
            Some(existing) => SplitTree::Split {
                id: uuid::Uuid::new_v4(),
                dir: SplitDir::Vertical,
                ratio: 0.5,
                a: Box::new(existing),
                b: Box::new(leaf),
            },
        });
    }
    let info = WorkspaceInfo {
        id: uuid::Uuid::new_v4(),
        number: 1,
        name: None,
        tree,
    };
    (info, grids)
}

fn bench_render() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    println!("\n== 4. TUI render cost (render::draw, full split-tree walk) ==");
    for &leaf_count in &[1usize, 4, 8] {
        let (workspace, grids) = build_split_workspace(leaf_count, 24, 80);
        let backend = TestBackend::new(200, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let iters = 100;
        let start = Instant::now();
        for _ in 0..iters {
            terminal
                .draw(|frame| render::draw(frame, &workspace, &grids, None))
                .unwrap();
        }
        println!(
            "  {leaf_count} leaf(ves): {:.1} us/op",
            avg_us(start.elapsed(), iters)
        );
    }
}

struct BenchConn {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

impl BenchConn {
    async fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path)
            .await
            .expect("connect to in-process bench daemon");
        let (reader, writer) = stream.into_split();
        Self { reader, writer }
    }

    async fn request(&mut self, req: Request) -> Response {
        dimax::protocol::framing::write_frame(&mut self.writer, &req)
            .await
            .expect("write request frame");
        loop {
            match dimax::protocol::framing::read_frame(&mut self.reader)
                .await
                .expect("read response frame")
            {
                ServerMessage::Response(r) => return r,
                ServerMessage::Event(_) => continue,
            }
        }
    }
}

struct BenchDaemon(std::path::PathBuf);

impl Drop for BenchDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn start_bench_daemon() -> BenchDaemon {
    let path = std::env::temp_dir().join(format!("dimax-bench-{}.sock", uuid::Uuid::new_v4()));
    dimax::daemon::run(path.clone())
        .await
        .expect("bench daemon should bind and start");
    BenchDaemon(path)
}

async fn bench_lock_contention() {
    println!("\n== 5. lock contention: 10 unrelated spawns while an unfocused pane floods ==");
    let guard = start_bench_daemon().await;
    let mut flooder = BenchConn::connect(&guard.0).await;
    let server = match flooder
        .request(Request::ServerSpawn {
            name: None,
            cmd: Some("yes".to_string()),
            cwd: None,
            workspace: None,
        })
        .await
    {
        Response::ServerPane(info) => info.id,
        other => panic!("expected ServerPane, got {other:?}"),
    };
    let ws = match flooder
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
    flooder
        .request(Request::Subscribe {
            workspace: ws.to_string(),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut other = BenchConn::connect(&guard.0).await;
    let start = Instant::now();
    for i in 0..10 {
        other
            .request(Request::ServerSpawn {
                name: Some(format!("bench-{i}")),
                cmd: None,
                cwd: None,
                workspace: None,
            })
            .await;
    }
    let elapsed = start.elapsed();
    println!(
        "  10 unrelated ServerSpawns while flooding: {:.1} ms total ({:.1} ms/op)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / 10.0
    );

    // Baseline for comparison: same 10 spawns, no flooding pane at all.
    drop(guard);
    let guard2 = start_bench_daemon().await;
    let mut baseline = BenchConn::connect(&guard2.0).await;
    let start = Instant::now();
    for i in 0..10 {
        baseline
            .request(Request::ServerSpawn {
                name: Some(format!("baseline-{i}")),
                cmd: None,
                cwd: None,
                workspace: None,
            })
            .await;
    }
    let elapsed = start.elapsed();
    println!(
        "  same 10 spawns, no flood (baseline):    {:.1} ms total ({:.1} ms/op)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / 10.0
    );
}

fn main() {
    // Plain `fn main` deliberately -- `spawn_filled_pane`'s
    // `rx.blocking_recv()` can't run inside a tokio runtime context
    // (panics: "Cannot block the current thread from within a runtime").
    // Only `bench_lock_contention` needs async, so it gets its own
    // runtime, built and torn down after the sync benches are done.
    bench_snapshot();
    bench_serialize();
    bench_broadcast_fanout();
    bench_render();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(bench_lock_contention());
}
