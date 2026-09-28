#![allow(clippy::unwrap_used, clippy::expect_used)]
// False positive: `BenchmarkGroup::finish` consumes the group, which this lint doesn't see
#![allow(clippy::significant_drop_tightening)]

//! Criterion benchmarks for the hot paths of a session.
//!
//! - `emulator`: parsing a colorful full-screen redraw into the alacritty `Term`.
//! - `render`: formatting the screen for `tui_read` and as plain text for screen-mode expect.
//! - `output`: stripping escapes and matching patterns in the session output buffer.
//! - `pty`: real PTY round trips through `PtyManager` (includes process and kernel time).
//!
//! All screens are 43x155, the default size for tests in this project.
//!
//! Run with `cargo bench`, or e.g. `cargo bench -- render` for one group.

use std::hint::black_box;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use shadowpty::formatter::{format_screen, screen_text};
use shadowpty::output::{Pattern, SessionOutput, plain_text};
use shadowpty::pty_manager::{ExpectTarget, Expectation, PtyConfig, PtyManager};

const ROWS: u16 = 43;
const COLS: u16 = 155;

struct ScreenSize;

impl Dimensions for ScreenSize {
    fn total_lines(&self) -> usize {
        ROWS as usize
    }
    fn screen_lines(&self) -> usize {
        ROWS as usize
    }
    fn columns(&self) -> usize {
        COLS as usize
    }
}

/// One full-screen redraw: every row repositioned and filled with styled segments
/// (bold, 256-color, truecolor, reset, underline + background).
fn frame(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[H");
    for row in 0..ROWS as usize {
        write!(out, "\x1b[{};1H", row + 1).unwrap();
        let mut col = 0;
        while col < COLS as usize {
            match (row * 7 + col * 3 + n) % 5 {
                0 => out.extend_from_slice(b"\x1b[1;31m"),
                1 => write!(out, "\x1b[38;5;{}m", (row + col + n) % 256).unwrap(),
                2 => write!(
                    out,
                    "\x1b[38;2;{};{};{}m",
                    row * 5 % 256,
                    col % 256,
                    n % 256
                )
                .unwrap(),
                3 => out.extend_from_slice(b"\x1b[0m"),
                _ => out.extend_from_slice(b"\x1b[4;44m"),
            }
            let len = 10.min(COLS as usize - col);
            for i in 0..len {
                out.push(b'a' + u8::try_from((row + col + i + n) % 26).unwrap());
            }
            col += len;
        }
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

fn new_term() -> Term<VoidListener> {
    Term::new(Config::default(), &ScreenSize, VoidListener)
}

fn drawn_term() -> Term<VoidListener> {
    let mut term = new_term();
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut term, &frame(3));
    term
}

fn bench_emulator(c: &mut Criterion) {
    let corpus: Vec<u8> = (0..50).flat_map(frame).collect();
    let mut group = c.benchmark_group("emulator");
    group.throughput(Throughput::Bytes(corpus.len() as u64));
    group.bench_function("parse_50_redraws", |b| {
        b.iter_batched(
            || (new_term(), Processor::<StdSyncHandler>::new()),
            |(mut term, mut parser)| {
                // Same chunk size as the PTY reader thread
                for chunk in corpus.chunks(4096) {
                    parser.advance(&mut term, chunk);
                }
                term
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_render(c: &mut Criterion) {
    let term = drawn_term();
    let mut group = c.benchmark_group("render");
    group.bench_function("format_screen", |b| {
        b.iter(|| format_screen(black_box(&term)));
    });
    group.bench_function("screen_text", |b| {
        b.iter(|| screen_text(black_box(&term)));
    });
    group.finish();
}

fn bench_output(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let redraw = frame(3);

    let mut group = c.benchmark_group("output");
    group.throughput(Throughput::Bytes(redraw.len() as u64));
    group.bench_function("plain_text_one_redraw", |b| {
        b.iter(|| plain_text(black_box(&redraw)));
    });

    // Worst case for a stream expect: the match sits at the end of a full 1 MiB buffer
    let filler: Vec<u8> = frame(1).into_iter().cycle().take(1 << 20).collect();
    group.throughput(Throughput::Bytes(filler.len() as u64));
    for (name, pattern) in [
        ("expect_literal_1mib", Pattern::literal("NEEDLE-4242").unwrap()),
        ("expect_regex_1mib", Pattern::regex(r"NEEDLE-\d{4}").unwrap()),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let output = SessionOutput::new();
                    output.push(&filler);
                    output.push(b"NEEDLE-4242");
                    output
                },
                |output| {
                    rt.block_on(output.expect(&pattern, Duration::from_secs(1)))
                        .unwrap()
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn bench_pty(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let manager = PtyManager::new();
    rt.block_on(manager.start_app(&PtyConfig::new("cat", &[], ROWS, COLS)))
        .unwrap();

    let mut group = c.benchmark_group("pty");
    group.measurement_time(Duration::from_secs(10));

    let counter = AtomicUsize::new(0);
    group.bench_function("input_to_expect_round_trip", |b| {
        b.to_async(&rt).iter(|| async {
            let marker = format!("M{}Z", counter.fetch_add(1, Ordering::Relaxed));
            manager
                .send_input(&format!("{marker}<ENTER>"))
                .await
                .unwrap();
            manager
                .expect(&Expectation {
                    pattern: Pattern::literal(&marker).unwrap(),
                    target: ExpectTarget::Stream,
                    timeout: Duration::from_secs(5),
                })
                .await
                .unwrap()
        });
    });

    group.bench_function("read_screen", |b| {
        b.to_async(&rt).iter(|| async { manager.read_screen().await.unwrap() });
    });
    group.finish();

    rt.block_on(manager.stop_app()).unwrap();
}

criterion_group!(
    benches,
    bench_emulator,
    bench_render,
    bench_output,
    bench_pty
);
criterion_main!(benches);
