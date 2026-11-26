#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `heph.gh.release_asset(...)` end to end: the function reached behind the
//! plugin ABI (exported with `make_plugin_functions`, loaded with
//! `decode_functions`, as a cdylib's is), declaring an `http_fetch` of the
//! asset's download URL — `server_url` pointed at a loopback server — and a
//! `gh.extract` that unpacks it. The declared targets are fetched, verified and
//! unpacked by the real engine.

use crate::http_fetch::serve;
use heph::engine::PluginParts;
use hplugin::function::PluginFnDef;
use std::io::{Read as _, Write as _};
use std::sync::atomic::Ordering;

fn behind_the_abi(plugin: &str, defs: Vec<PluginFnDef>) -> anyhow::Result<Vec<PluginFnDef>> {
    let (named, handle) = hplugin_sdk::stabby::make_plugin_functions(plugin, defs)?;
    hplugin_stabby::load_stable::decode_functions(plugin, named.into_iter().collect(), handle)
}

/// A workspace with BUILD files, `http_fetch`, and the `gh` plugin's function
/// and `extract` driver. The BUILD files pass `server_url`.
fn workspace() -> anyhow::Result<htestkit::Workspace> {
    htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginhttp::Driver))
        .with_plugin("gh", |_| {
            Ok(PluginParts::default()
                .with_functions(behind_the_abi("gh", hplugin_gh::release::functions("gh"))?)
                .with_managed_driver(Box::new(hplugin_gh::extract::Driver)))
        })
        .build()
}

/// A release tarball the way most projects ship one: everything under one
/// top-level directory.
fn tarball() -> &'static [u8] {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data, mode) in [
        ("tool-1.0/bin/hello", &b"#!/bin/sh\necho hello\n"[..], 0o755),
        ("tool-1.0/README", &b"readme\n"[..], 0o644),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, path, data).expect("append");
    }
    let tar = b.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).expect("gzip");
    gz.finish().expect("gzip").leak()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Every file of a result, by path, with its contents and executable bit.
fn files(result: &heph::engine::EResult) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk") {
            let entry = entry.expect("entry");
            if let heph::hartifactcontent::WalkEntryKind::File { mut data, x, .. } = entry.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("utf8 file");
                out.push((entry.path.to_string_lossy().into_owned(), s, x));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn release_asset_fetches_and_extracts_a_tarball() -> anyhow::Result<()> {
    let body = tarball();
    let (url, hits) = serve(body, 4);
    let ws = workspace()?;
    ws.write_build_file(
        "third_party/tool",
        &format!(
            r#"
tool = heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.0",
    asset = "tool-1.0.tar.gz",
    sha256 = "{}",
    extract = True,
    strip_components = 1,
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );

    let spec = ws.get_spec("//third_party/tool:tool").await?;
    assert_eq!(spec.driver, "gh.extract");
    assert_eq!(
        ws.get_spec("//third_party/tool:tool_archive").await?.driver,
        "http_fetch"
    );

    let result = ws.run("//third_party/tool:tool").await?;
    assert_eq!(
        files(&result),
        vec![
            (
                "third_party/tool/tool/README".to_string(),
                "readme\n".to_string(),
                false
            ),
            (
                "third_party/tool/tool/bin/hello".to_string(),
                "#!/bin/sh\necho hello\n".to_string(),
                true
            ),
        ]
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch, one request");
    Ok(())
}

#[tokio::test]
async fn release_asset_with_a_wrong_sha256_fails() -> anyhow::Result<()> {
    let (url, _hits) = serve(tarball(), 2);
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.0",
    asset = "tool-1.0.tar.gz",
    sha256 = "0000000000000000000000000000000000000000000000000000000000000000",
    extract = True,
    server_url = "{url}",
)
"#
        ),
    );
    let Err(err) = ws.run("//p:tool").await else {
        panic!("a checksum mismatch must fail the build");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("checksum mismatch"), "got: {msg}");
    Ok(())
}

#[tokio::test]
async fn release_asset_without_extract_fetches_the_file() -> anyhow::Result<()> {
    const SCRIPT: &[u8] = b"#!/bin/sh\necho fetched\n";
    let (url, _hits) = serve(SCRIPT, 2);
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "release/v2",
    asset = "tool.sh",
    sha256 = "{}",
    executable = True,
    server_url = "{url}",
)
"#,
            sha256(SCRIPT)
        ),
    );
    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result),
        vec![(
            "p/tool.sh".to_string(),
            "#!/bin/sh\necho fetched\n".to_string(),
            true
        )]
    );
    Ok(())
}

/// The def hash of `//p:<target>` in a fresh workspace whose `p/BUILD` is one
/// `release_asset` call with `extra` arguments. Each workspace is its own
/// engine, so two calls are compared under the same address.
async fn def_hash(extra: &str, target: &str) -> anyhow::Result<Vec<u8>> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.0",
    asset = "tool.sh",
    {extra}
)
"#
        ),
    );
    let addr = heph::htaddr::parse_addr(&format!("//p:{target}"))?;
    let def = std::sync::Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    Ok(def.target_def.hash.clone())
}

/// The server is a per-call argument, and it reaches the cache key: two calls
/// differing only by `server_url` declare fetches with different def hashes,
/// so an asset from one server is never served from another's cache entry.
/// The same call twice hashes the same, so the difference is the server's.
#[tokio::test]
async fn server_url_reaches_the_fetch_def_hash() -> anyhow::Result<()> {
    let ghe = def_hash(r#"server_url = "https://ghe.example","#, "tool").await?;
    assert_eq!(
        ghe,
        def_hash(r#"server_url = "https://ghe.example/","#, "tool").await?,
        "a trailing `/` is the same server"
    );
    assert_ne!(ghe, def_hash("", "tool").await?);
    assert_ne!(
        ghe,
        def_hash(r#"server_url = "https://ghe2.example","#, "tool").await?
    );
    Ok(())
}
