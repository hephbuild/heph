//! Cross-backend behavior tests — exercise the filesystem backend through the
//! same trait surface as the in-memory one (the inline per-module tests cover
//! the in-memory bridge in detail).

use crate::hasync::StdCancellationToken;
use crate::hlock::traits::{TLock, TReadGuard, TWriteGuard};

fn ct() -> StdCancellationToken {
    StdCancellationToken::new()
}

#[tokio::test]
async fn fs_bridge_upgrade_then_downgrade() {
    let dir = tempfile::tempdir().expect("tempdir");
    let l = crate::hlock::fs_tlock(dir.path().join("outer"), dir.path().join("inner"));

    // read -> upgrade -> exclusive
    let r = l.read(&ct()).await.unwrap();
    let w = r.upgrade(&ct()).await.unwrap();
    assert!(l.try_read().unwrap().is_none(), "exclusive after upgrade");

    // write -> downgrade -> readers admitted, writers excluded
    let r = w.downgrade(&ct()).await.unwrap();
    assert!(
        l.try_read().unwrap().is_some(),
        "readers admitted after downgrade"
    );
    assert!(l.try_write().unwrap().is_none(), "writer still excluded");

    drop(r);
    assert!(
        l.try_write().unwrap().is_some(),
        "writer after readers drain"
    );
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
