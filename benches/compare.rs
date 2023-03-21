use std::{hint::black_box, time::Duration};

use criterion::{criterion_group, criterion_main, Bencher, BenchmarkId, Criterion};

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

trait BencherExt {
    fn iter_setup<R>(&mut self, res: R, setup: impl FnMut(&mut R), bench: impl FnMut(&mut R));
}

impl BencherExt for Bencher<'_> {
    fn iter_setup<R>(
        &mut self,
        mut res: R,
        mut setup: impl FnMut(&mut R),
        mut bench: impl FnMut(&mut R),
    ) {
        self.iter_custom(|iters| {
            let mut total = Duration::default();

            for _ in 0..iters {
                setup(&mut res);
                let start = std::time::Instant::now();
                bench(&mut res);
                total += start.elapsed();
            }

            total
        });
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

fn recv_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("recv only");

    for i in [16, 32, 64, 128, 256, 512] {
        group.bench_function(BenchmarkId::new("batch", i), |b| {
            b.iter_setup(
                batch_queue::channel(i * 2),
                |(_, tx)| {
                    for i in 0..i {
                        tx.try_send(i).unwrap();
                    }
                },
                |(rx, _)| {
                    let mut got = 0;
                    while got < i {
                        let rx = black_box(rx.recv());

                        got += rx.len();

                        for i in rx {
                            let _i = black_box(i);
                        }
                    }
                },
            )
        });

        group.bench_function(BenchmarkId::new("mpsc", i), |b| {
            b.iter_setup(
                std::sync::mpsc::sync_channel(i),
                |(tx, _)| {
                    for i in 0..i {
                        tx.try_send(i).unwrap();
                    }
                },
                |(_, rx)| {
                    for i in 0..i {
                        let it = black_box(black_box(&rx).try_recv()).unwrap();
                    }
                },
            )
        });
    }

    group.finish();
}

// TODO(josiah) use custom iter functions to only measure the render thread time to get each batch.

criterion_group!(group, total_time, recv_time);
criterion_main!(group);
