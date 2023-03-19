use super::*;

#[test]
fn auto_traits() {
    static_assertions::assert_impl_all!(
        Rx<usize>: Send
    );

    static_assertions::assert_not_impl_any!(
        Rx<usize>: Sync
    );

    static_assertions::assert_impl_all!(
        Tx<usize>: Send, Sync
    );
}

fn rx_all<T: Send>(rx: &mut Rx<T>) -> Vec<T> {
    let mut out = Vec::new();

    loop {
        out.extend(rx.recv());

        if !rx.may_rx() {
            break;
        }
    }

    out
}

#[cfg(miri)]
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
