use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn spsc_push_pop(c: &mut Criterion) {
    c.bench_function("spsc/push_pop", |b| {
        let (mut p, mut con) = aster_transport_core::ring::spsc::<u64>(256);
        b.iter(|| {
            p.try_push(black_box(42)).unwrap();
            black_box(con.try_pop().unwrap());
        });
    });
}

fn tokio_mpsc_try_send_recv(c: &mut Criterion) {
    c.bench_function("tokio_mpsc/try_send_recv", |b| {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(256);
        b.iter(|| {
            tx.try_send(black_box(42)).unwrap();
            black_box(rx.try_recv().unwrap());
        });
    });
}

fn spsc_batch_drain(c: &mut Criterion) {
    for batch in [8, 32, 64] {
        c.bench_function(&format!("spsc/drain_{batch}"), |b| {
            let (mut p, mut con) = aster_transport_core::ring::spsc::<u64>(256);
            for i in 0..batch {
                p.try_push(i as u64).unwrap();
            }
            b.iter(|| {
                con.drain(batch, |v| {
                    black_box(v);
                });
                for i in 0..batch {
                    p.try_push(i as u64).unwrap();
                }
            });
        });
    }
}

fn spsc_cross_thread(c: &mut Criterion) {
    let n = 100_000u64;
    c.bench_function("spsc/cross_thread_100k", |b| {
        b.iter(|| {
            let (mut p, mut con) = aster_transport_core::ring::spsc::<u64>(256);

            let producer = std::thread::spawn(move || {
                for i in 0..n {
                    while p.try_push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });

            let mut count = 0u64;
            while count < n {
                if con.try_pop().is_some() {
                    count += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            producer.join().unwrap();
            black_box(count);
        });
    });
}

fn tokio_mpsc_cross_thread(c: &mut Criterion) {
    let n = 100_000u64;
    c.bench_function("tokio_mpsc/cross_thread_100k", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(256);

            let producer = std::thread::spawn(move || {
                for i in 0..n {
                    while tx.try_send(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });

            let mut count = 0u64;
            while count < n {
                if rx.try_recv().is_ok() {
                    count += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            producer.join().unwrap();
            black_box(count);
        });
    });
}

fn spsc_cross_thread_drain(c: &mut Criterion) {
    let n = 100_000u64;
    c.bench_function("spsc/cross_thread_drain32_100k", |b| {
        b.iter(|| {
            let (mut p, mut con) = aster_transport_core::ring::spsc::<u64>(256);

            let producer = std::thread::spawn(move || {
                for i in 0..n {
                    while p.try_push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });

            let mut count = 0u64;
            while count < n {
                let drained = con.drain(32, |_| count += 1);
                if drained == 0 {
                    std::hint::spin_loop();
                }
            }
            producer.join().unwrap();
            black_box(count);
        });
    });
}

criterion_group!(
    benches,
    spsc_push_pop,
    tokio_mpsc_try_send_recv,
    spsc_batch_drain,
    spsc_cross_thread,
    tokio_mpsc_cross_thread,
    spsc_cross_thread_drain,
);
criterion_main!(benches);
