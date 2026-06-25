//! End-to-end tests for the FUSE sandbox overlay.
//!
//! These force `fuse: { enabled: true }` and exercise the real mount path:
//! read-only dep outputs are served straight from the seekable tar bytes via
//! `TarIndex`, while a target's own writes land in the on-disk upper layer.
//!
//! Every test gates on [`common::fuse_mount_works`] and skips (rather than
//! fails) when the host has no usable FUSE — macFUSE present but its system
//! extension unapproved on macOS, or `/dev/fuse` missing in a container. On
//! Linux CI the mount succeeds and the bodies run for real.
//!
//! Note: a dep's output is materialized in the consumer's sandbox at
//! `<dep_package>/<out_name>`, so tests keep dep and consumer in distinct
//! packages with distinct output filenames to avoid path collisions.

mod common;

/// Skip the enclosing test when a real FUSE mount can't be brought up here.
macro_rules! require_fuse {
    () => {
        if !common::fuse_mount_works() {
            eprintln!("skipping: no usable FUSE mount on this host");
            return Ok(());
        }
    };
}

// A consumer reads its dependency's output. With FUSE on, the dep bytes are
// served from the tar layer through the mount — this is the exact path that
// used to abort with "RefCell already mutably borrowed" because the pooled
// sqlite connection backing the seekable reader was moved out from under the
// blob that borrowed it.
#[tokio::test]
async fn test_fuse_dep_output_read_through_layer() -> anyhow::Result<()> {
    require_fuse!();
    let ws = common::Workspace::with_fuse();
    ws.write_build_file(
        "libpkg",
        r#"target(name = "lib", driver = "bash", run = "printf LIBDATA-9999 > $OUT", out = "lib.txt")"#,
    );
    ws.write_build_file(
        "apppkg",
        r#"target(name = "app", driver = "bash", run = "cat $SRC_LIB > $OUT", out = "app.txt", deps = {"lib": ["//libpkg:lib"]})"#,
    );

    let result = ws.run("//apppkg:app").await?;
    let content = common::artifact_string(&result);
    assert_eq!(
        content, "LIBDATA-9999",
        "dep bytes served via FUSE layer, got: {content:?}"
    );
    Ok(())
}

// A large dep output forces multi-block reads (>64 KiB copy chunks and many
// FUSE read ops). Verifies byte-exact streaming from the layer, not just that
// the mount comes up.
#[tokio::test]
async fn test_fuse_large_dep_read_is_byte_exact() -> anyhow::Result<()> {
    require_fuse!();
    let ws = common::Workspace::with_fuse();
    // head reads directly from the /dev/zero file (no upstream pipe), so no
    // SIGPIPE; tr maps NULs to 'A'. 500000 bytes >> one FUSE read.
    ws.write_build_file(
        "biglib",
        r#"target(name = "lib", driver = "bash", run = "head -c 500000 /dev/zero | tr '\\0' 'A' > $OUT", out = "big.txt")"#,
    );
    ws.write_build_file(
        "bigapp",
        r#"target(name = "app", driver = "bash", run = "wc -c < $SRC_LIB | tr -d ' \n' > $OUT; printf : >> $OUT; cksum $SRC_LIB | cut -d' ' -f1 | tr -d '\n' >> $OUT", out = "app.txt", deps = {"lib": ["//biglib:lib"]})"#,
    );

    let result = ws.run("//bigapp:app").await?;
    let content = common::artifact_string(&result);
    let (count, sum) = content.split_once(':').unwrap_or(("", ""));
    assert_eq!(count, "500000", "byte count via FUSE, got: {content:?}");
    // cksum over 500000 'A' bytes is deterministic on coreutils.
    assert!(!sum.is_empty(), "expected a checksum, got: {content:?}");
    Ok(())
}

// A target writes a file, then reads it back within the same run. The write
// lands in the FUSE upper layer (copy-on-write) and the read must see it —
// exercises the passthrough write + upper-wins read path.
#[tokio::test]
async fn test_fuse_write_then_read_back_from_upper() -> anyhow::Result<()> {
    require_fuse!();
    let ws = common::Workspace::with_fuse();
    ws.write_build_file(
        "w",
        r#"target(name = "t", driver = "bash", run = "echo scratch_payload > scratch.txt; cat scratch.txt > $OUT", out = "out.txt")"#,
    );

    let result = ws.run("//w:t").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("scratch_payload"),
        "upper-layer write should read back, got: {content:?}"
    );
    Ok(())
}

// Two consumers read the same dep concurrently. Each FUSE read opens its own
// pooled sqlite connection; this guards the multi-threaded read path (Linux
// runs the session multi-threaded) against connection-sharing regressions.
#[tokio::test]
async fn test_fuse_shared_dep_read_by_two_consumers() -> anyhow::Result<()> {
    require_fuse!();
    let ws = common::Workspace::with_fuse();
    ws.write_build_file(
        "leafpkg",
        r#"target(name = "leaf", driver = "bash", run = "printf LEAFBYTES > $OUT", out = "leaf.txt")"#,
    );
    ws.write_build_file(
        "fan",
        r#"
target(name = "a", driver = "bash", run = "cat $SRC_LEAF > $OUT", out = "a.txt", deps = {"leaf": ["//leafpkg:leaf"]})
target(name = "b", driver = "bash", run = "cat $SRC_LEAF > $OUT", out = "b.txt", deps = {"leaf": ["//leafpkg:leaf"]})
target(
    name = "root",
    driver = "bash",
    run = "printf '%s|%s' \"$(cat $SRC_A)\" \"$(cat $SRC_B)\" > $OUT",
    out = "root.txt",
    deps = {"a": ["//fan:a"], "b": ["//fan:b"]},
)
"#,
    );

    let result = ws.run("//fan:root").await?;
    let content = common::artifact_string(&result);
    assert_eq!(content, "LEAFBYTES|LEAFBYTES", "got: {content:?}");
    Ok(())
}
