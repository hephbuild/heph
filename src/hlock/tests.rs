//! Cross-backend behavior tests — exercise the filesystem backend through the
//! same trait surface as the in-memory one (the inline per-module tests cover
//! the in-memory bridge in detail).

use crate::hasync::StdCancellationToken;
use crate::hlock::traits::{TLock, TUpgradableReadGuard, TWriteGuard};
use std::sync::Arc;
use std::time::Duration;

fn ct() -> StdCancellationToken {
    StdCancellationToken::new()
}

#[tokio::test]
async fn fs_bridge_upgrade_then_downgrade() {
    let dir = tempfile::tempdir().expect("tempdir");
    let l = crate::hlock::fs_tlock(dir.path().join("outer"), dir.path().join("inner"));

    // upgradable read -> upgrade -> exclusive
    let r = l.upgradable_read(&ct()).await.unwrap();
    let w = r.upgrade(&ct()).await.unwrap();
    assert!(l.try_read().unwrap().is_none(), "exclusive after upgrade");

    // write -> downgrade -> readers admitted, writers excluded
    let u = w.downgrade(&ct()).await.unwrap();
    assert!(
        l.try_read().unwrap().is_some(),
        "readers admitted after downgrade"
    );
    assert!(l.try_write().unwrap().is_none(), "writer still excluded");

    drop(u);
    assert!(
        l.try_write().unwrap().is_some(),
        "writer after readers drain"
    );
}

// Two independent `fs_tlock` instances on the same files model two processes
// upgrading concurrently. Under the old lazy-outer acquisition this circular-
// waited; holding the outer gateway across the upgradable lifetime serializes
// them cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fs_two_upgraders_no_deadlock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outer = Arc::new(dir.path().join("outer"));
    let inner = Arc::new(dir.path().join("inner"));

    let spawn_upgrader = || {
        let outer = Arc::clone(&outer);
        let inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let l = crate::hlock::fs_tlock(&*outer, &*inner);
            let u = l.upgradable_read(&ct()).await.unwrap();
            let w = u.upgrade(&ct()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(w);
        })
    };

    let a = spawn_upgrader();
    let b = spawn_upgrader();

    tokio::time::timeout(Duration::from_secs(10), async {
        a.await.unwrap();
        b.await.unwrap();
    })
    .await
    .expect("two fs upgraders must not deadlock");
}

#[tokio::test]
async fn fs_bridge_writers_serialize_across_instances() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outer = dir.path().join("outer");
    let inner = dir.path().join("inner");
    let a = crate::hlock::fs_tlock(&outer, &inner);
    let b = crate::hlock::fs_tlock(&outer, &inner);

    let _w = a.write(&ct()).await.unwrap();
    assert!(
        b.try_write().unwrap().is_none(),
        "second writer blocked across instances"
    );
}
