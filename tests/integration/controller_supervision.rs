//! The supervisor of the daemon's local controller loop (U2, KTD4): a loop
//! that dies is started again, and shutdown waits — within a limit — for the
//! loop to give its lease up.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use narsor_controller_runtime::{supervise, Supervision};
use tokio::sync::oneshot;

const FAST: Supervision = Supervision {
    restart_after: Duration::from_millis(10),
    stop_timeout: Duration::from_millis(300),
};

#[tokio::test]
async fn a_loop_that_panics_is_started_again() {
    let starts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&starts);
    let (stop, shutdown) = oneshot::channel::<()>();

    let supervisor = tokio::spawn(supervise(
        move |stopped| {
            let attempt = counted.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 3 {
                    panic!("the loop dies on attempt {attempt}");
                }
                let _ = stopped.await;
            }
        },
        async move {
            let _ = shutdown.await;
        },
        FAST,
    ));

    tokio::time::timeout(Duration::from_secs(5), async {
        while starts.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the loop was restarted after each panic");

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), supervisor)
        .await
        .expect("the supervisor stops on shutdown")
        .unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 3, "no start after shutdown");
}

#[tokio::test]
async fn shutdown_waits_for_the_loop_to_release_before_returning() {
    let released = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&released);
    let (stop, shutdown) = oneshot::channel::<()>();

    let supervisor = tokio::spawn(supervise(
        move |stopped| {
            let flag = Arc::clone(&flag);
            async move {
                let _ = stopped.await;
                // The lease release happens after the stop signal.
                tokio::time::sleep(Duration::from_millis(50)).await;
                flag.store(true, Ordering::SeqCst);
            }
        },
        async move {
            let _ = shutdown.await;
        },
        FAST,
    ));

    tokio::time::sleep(Duration::from_millis(20)).await;
    stop.send(()).unwrap();
    supervisor.await.unwrap();

    assert!(
        released.load(Ordering::SeqCst),
        "the supervisor returned only after the loop finished releasing"
    );
}

#[tokio::test]
async fn a_loop_that_ignores_the_stop_signal_is_abandoned_after_the_timeout() {
    let (stop, shutdown) = oneshot::channel::<()>();

    let supervisor = tokio::spawn(supervise(
        |_stopped| std::future::pending::<()>(),
        async move {
            let _ = shutdown.await;
        },
        FAST,
    ));

    tokio::time::sleep(Duration::from_millis(20)).await;
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), supervisor)
        .await
        .expect("a stuck loop cannot hold the daemon's shutdown hostage")
        .unwrap();
}

#[tokio::test]
async fn shutdown_during_the_pause_before_a_restart_does_not_start_another_loop() {
    let starts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&starts);
    let (stop, shutdown) = oneshot::channel::<()>();

    let slow = Supervision {
        restart_after: Duration::from_secs(30),
        stop_timeout: Duration::from_millis(300),
    };
    let supervisor = tokio::spawn(supervise(
        move |_stopped| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { panic!("dies at once") }
        },
        async move {
            let _ = shutdown.await;
        },
        slow,
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), supervisor)
        .await
        .expect("shutdown interrupts the restart pause")
        .unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}
