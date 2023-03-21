use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};

fn on_render(fps: u64, mut block: impl FnMut() -> bool) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(1000 / fps));

        if !block() {
            break;
        }
    }
}

fn batch<const N: usize>(fps: u64, cap: usize, each_send: usize) {
    let (mut rx, tx) = batch_queue::channel(cap);

    let ts: [_; N] = std::array::from_fn({
        |_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for i in 0..each_send {
                    tx.blocking_send(i).unwrap();
                }
            })
        }
    });

    drop(tx);

    on_render(fps, move || {
        for it in rx.recv() {
            let it = black_box(it);
        }

        rx.may_rx()
    });

    for t in ts {
        t.join().unwrap();
    }
}

fn std_mpsc<const N: usize>(fps: u64, cap: usize, each_send: usize) {
    let (tx, rx) = std::sync::mpsc::sync_channel(cap);

    let ts: [_; N] = std::array::from_fn({
        |_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for i in 0..each_send {
                    tx.send(i).unwrap();
                }
            })
        }
    });

    drop(tx);

    on_render(fps, move || {
        let mut got = false;

        for it in std::iter::from_fn(|| rx.try_recv().ok()).take(cap).fuse() {
            got = true;
            let it = black_box(it);
        }

        got
    });

    for t in ts {
        t.join().unwrap();
    }
}

fn total_time(c: &mut Criterion) {
    c.benchmark_group("60 fps - 1 sender - 128 items - 32 cap")
        .bench_function("batch", |b| b.iter(|| batch::<1>(60, 32, 128)))
        .bench_function("mpsc", |b| b.iter(|| std_mpsc::<1>(60, 32, 128)));

    c.benchmark_group("60 fps - 2 sender - 128 items - 32 cap")
        .bench_function("batch", |b| b.iter(|| batch::<2>(60, 32, 128)))
        .bench_function("mpsc", |b| b.iter(|| std_mpsc::<2>(60, 32, 128)));

    c.benchmark_group("60 fps - 1 sender - 128 items - 32 cap (staggered)")
        .bench_function("batch", |b| b.iter(|| batch::<1>(60, 64, 128)))
        .bench_function("mpsc", |b| b.iter(|| std_mpsc::<1>(60, 32, 128)));

    c.benchmark_group("60 fps - 2 sender - 128 items - 32 cap (staggered)")
        .bench_function("batch", |b| b.iter(|| batch::<2>(60, 64, 128)))
        .bench_function("mpsc", |b| b.iter(|| std_mpsc::<2>(60, 32, 128)));

    c.benchmark_group("60 fps - 16 sender - 128 items - 32 cap (staggered)")
        .bench_function("batch", |b| b.iter(|| batch::<16>(60, 64, 128)))
        .bench_function("mpsc", |b| b.iter(|| std_mpsc::<16>(60, 32, 128)));
}

// TODO(josiah) use custom iter functions to only measure the render thread time to get each batch.

criterion_group!(group, total_time);
criterion_main!(group);
