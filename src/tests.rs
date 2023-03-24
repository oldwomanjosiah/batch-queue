use std::sync::atomic::AtomicUsize;

use super::*;

#[test]
fn auto_traits() {
    static_assertions::assert_impl_all!(
        Receiver<usize>: Send
    );

    static_assertions::assert_not_impl_any!(
        Receiver<usize>: Sync
    );

    static_assertions::assert_impl_all!(
        Sender<usize>: Send, Sync
    );
}

fn rx_all<T: Send>(rx: &mut Receiver<T>) -> Vec<T> {
    let mut out = Vec::new();

    loop {
        let it = rx.recv();

        eprintln!("rx_all: {} items", it.len());

        out.extend(it);

        if !rx.may_recv() {
            break;
        }
    }

    out
}

#[test]
fn create_drop() {
    channel::<()>(16);
}

#[test]
fn send_once() {
    let (mut rx, tx) = channel(16);

    let t = std::thread::spawn(move || {
        tx.blocking_send(2).unwrap();
    });

    let received = rx_all(&mut rx);

    assert_eq!(received, &[2]);

    t.join().unwrap();
}

#[test]
fn always_swap() {
    let (mut rx, tx) = channel(2);

    let t = std::thread::spawn(move || {
        for i in 0..128 {
            tx.blocking_send(i).unwrap();
        }
    });

    let received = rx_all(&mut rx);

    let expected = {
        let mut it = Vec::with_capacity(128);
        it.extend(0..128);
        it
    };

    assert_eq!(received, expected);

    t.join().unwrap();
}

#[test]
fn always_fill() {
    let batch_size = 4;
    let total = 64 * batch_size;

    let (mut rx, tx) = channel(batch_size * 2);
    let sent = Arc::new(AtomicUsize::new(0));

    let t = std::thread::spawn({
        let sent = sent.clone();
        move || {
            for i in 0..total {
                tx.blocking_send(i).unwrap();
                sent.fetch_add(1, Ordering::Release);
            }
        }
    });

    for batch in 0..(total / batch_size) {
        while sent.load(Ordering::Acquire) < (batch + 1) * batch_size {
            std::hint::spin_loop();
        }

        let start = batch * batch_size;
        let expected: Vec<_> = (start..(start + batch_size)).collect();
        let mut curr = Vec::new();
        loop {
            let rem = batch_size - curr.len();
            if rem == 0 {
                break;
            }
            curr.extend(rx.recv().take(rem));
        }

        assert_eq!(curr, expected);
    }

    t.join().unwrap();
}

#[test]
fn send_many() {
    let (mut rx, tx) = channel(16);

    let t = std::thread::spawn(move || {
        for i in 0..2048 {
            tx.blocking_send(i).unwrap();
        }
    });

    let received = rx_all(&mut rx);

    let expected = {
        let mut it = Vec::with_capacity(2048);
        it.extend(0..2048);
        it
    };

    assert_eq!(received, expected);

    t.join().unwrap();
}

#[test]
fn fill_once() {
    let (mut rx, tx) = channel(16 * 2);

    let t = std::thread::spawn(move || {
        for i in 0..16 {
            tx.blocking_send(i).unwrap();
        }
    });

    {
        let block = rx.shared.get_block().unwrap();
        loop {
            let locked = block.locked.load(Ordering::Acquire);

            if locked == 16 {
                break;
            }

            std::hint::spin_loop();
        }
    }

    let actual: Vec<_> = rx_all(&mut rx);

    t.join().unwrap();

    let expected: Vec<_> = (0..16).collect();

    assert_eq!(actual, expected);
}

#[test]
fn fanout_send() {
    let (mut rx, tx) = channel(32);

    let ts = (0..16)
        .map(|id| {
            let tx = tx.clone();

            std::thread::Builder::new()
                .name(format!("Sender: {id}"))
                .spawn(move || {
                    for i in 0..16 {
                        tx.blocking_send(i + 16 * id).unwrap();
                    }
                })
                .unwrap()
        })
        .collect::<Vec<_>>();

    drop(tx);

    let actual = rx_all(&mut rx);

    for t in ts {
        t.join().unwrap()
    }

    for i in 0..(16 * 16) {
        assert!(actual.contains(&i));
    }
}

#[test]
fn unreceived_dropped() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Default)]
    struct NeedsDrop {
        been_dropped: bool,
    }

    impl Drop for NeedsDrop {
        fn drop(&mut self) {
            // Check that consuming logic is correct
            // This assert will only fail if we accidentally drop any already dropped item.
            assert!(!self.been_dropped, "Instance was dropped multiple times");
            self.been_dropped = true;

            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let (mut rx, tx) = channel(32);

    let t = std::thread::spawn(move || {
        for _ in 0..32 {
            tx.blocking_send(NeedsDrop::default()).unwrap();
        }
    });

    {
        let block = rx.shared.get_block().unwrap();

        loop {
            let locked = block.locked.load(Ordering::Acquire);

            if locked == 16 {
                break;
            }

            std::hint::spin_loop();
        }
    }

    let mut out = Vec::with_capacity(16);

    // By only taking half a batch here we are testing that partially consumed batches have their
    // contents correcty dropped.
    while out.len() < 8 {
        let rx = rx.recv();
        out.extend(rx.take(8 - out.len()));
    }

    // Join so that we wait for it to finish writing into the newly shared shared block,
    // Tests that we drop shared items as well.
    t.join().unwrap();
    drop(rx);

    assert_eq!(DROPS.load(Ordering::Acquire), 32 - 8);
    drop(out);
    assert_eq!(DROPS.load(Ordering::Acquire), 32);
}
