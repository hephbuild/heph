#![expect(
    clippy::panic_in_result_fn,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `heph.gh.release_asset(...)` end to end: the function reached behind the
//! plugin ABI (exported with `make_plugin_functions`, loaded with
//! `decode_functions`, as a cdylib's is), declaring an `http_fetch` of the
//! asset's download URL — `server_url` pointed at a loopback server — and a
//! builtin `archive.extract` that unpacks it. The declared targets are fetched, verified and
//! unpacked by the real engine.
//!
//! With `credentials`, `api_url` points at a loopback server playing the
//! GitHub API ([`serve_api`]): the release JSON is fetched, `gh.asset_url`
//! picks the asset's API URL out of it, and the asset fetch reads that URL
//! through `${read://…}`.

use crate::http_fetch::serve;
use heph::engine::PluginParts;
use hplugin::function::PluginFnDef;
use std::io::{BufRead as _, Read as _, Write as _};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

fn behind_the_abi(plugin: &str, defs: Vec<PluginFnDef>) -> anyhow::Result<Vec<PluginFnDef>> {
    let (named, handle) = hplugin_sdk::stabby::make_plugin_functions(plugin, defs)?;
    hplugin_stabby::load_stable::decode_functions(plugin, named.into_iter().collect(), handle)
}

/// A workspace with BUILD files, `http_fetch`, `bash`, the builtin `archive`
/// plugin, and the `gh` plugin's function and `asset_url` driver. The BUILD files pass
/// `server_url` (and `api_url`).
fn workspace() -> anyhow::Result<htestkit::Workspace> {
    crate::archive::archive_plugin(htestkit::WorkspaceBuilder::new()?)
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginhttp::Driver))
        .with_managed_driver(Box::new(
            heph::pluginexec::Driver::new_bash().with_host_path(),
        ))
        .with_plugin("gh", |_| {
            Ok(PluginParts::default()
                .with_functions(behind_the_abi("gh", hplugin_gh::release::functions("gh"))?)
                .with_managed_driver(Box::new(hplugin_gh::asset_url::Driver)))
        })
        .build()
}

/// A release tarball the way most projects ship one: everything under one
/// top-level directory.
fn tarball() -> &'static [u8] {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&plain_tar()).expect("gzip");
    gz.finish().expect("gzip").leak()
}

/// [`tarball`]'s tar, xz-compressed.
fn tar_xz() -> &'static [u8] {
    let opts = lzma_rust2::XzOptions::with_preset(1);
    let mut xz = lzma_rust2::XzWriter::new(Vec::new(), opts).expect("xz");
    xz.write_all(&plain_tar()).expect("xz");
    xz.finish().expect("xz").leak()
}

/// [`tarball`]'s tar, uncompressed.
fn plain_tar() -> Vec<u8> {
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
    b.into_inner().expect("tar")
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
    assert_eq!(spec.driver, "archive.extract");
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

/// A `.tar.xz` asset: the format is inferred from the name, and the tree
/// unpacks with its modes.
#[tokio::test]
async fn release_asset_extracts_a_tar_xz() -> anyhow::Result<()> {
    let body = tar_xz();
    let (url, _hits) = serve(body, 2);
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.0",
    asset = "tool-1.0.tar.xz",
    sha256 = "{}",
    extract = True,
    strip_components = 1,
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );
    assert_eq!(
        ws.get_spec("//p:tool").await?.config.get("format"),
        Some(&heph::htvalue::Value::String("tar.xz".to_string()))
    );
    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result),
        vec![
            ("p/tool/README".to_string(), "readme\n".to_string(), false),
            (
                "p/tool/bin/hello".to_string(),
                "#!/bin/sh\necho hello\n".to_string(),
                true
            ),
        ]
    );
    Ok(())
}

/// A binary shipped as one gzipped file (`tool-linux.gz`, as rust-analyzer
/// ships): it unpacks to one file named after the asset minus `.gz`, and
/// `executable = True` makes it runnable, since gzip records no mode.
#[tokio::test]
async fn release_asset_extracts_a_bare_gz_as_an_executable() -> anyhow::Result<()> {
    const BIN: &[u8] = b"#!/bin/sh\necho gunzipped\n";
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(BIN)?;
    let body: &'static [u8] = gz.finish()?.leak();
    let (url, _hits) = serve(body, 4);
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
[
    heph.gh.release_asset(
        name = name,
        repo = "acme/tool",
        tag = "v1.0",
        asset = "tool-linux.gz",
        sha256 = "{sha}",
        extract = True,
        executable = exe,
        server_url = "{url}",
    )
    for name, exe in [("tool", True), ("data", False)]
]
"#,
            sha = sha256(body)
        ),
    );
    for (addr, x) in [("//p:tool", true), ("//p:data", false)] {
        let result = ws.run(addr).await?;
        let dir = addr.trim_start_matches("//p:");
        assert_eq!(
            files(&result),
            vec![(
                format!("p/{dir}/tool-linux"),
                "#!/bin/sh\necho gunzipped\n".to_string(),
                x
            )],
            "{addr}"
        );
    }
    Ok(())
}

/// A zip built on Windows holding `tool` and `README.md`: no entry records a
/// Unix mode. Each central-directory record is marked as made on MS-DOS
/// (version made by, offset 5, system byte 0) with the archive attribute
/// only (external attributes, offset 38).
#[expect(
    clippy::indexing_slicing,
    reason = "test fixture: a record the writer just laid out; an offset past the end is a \
              broken fixture, and the panic says so"
)]
fn windows_zip() -> &'static [u8] {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in [
        ("tool", &b"#!/bin/sh\necho from-the-zip\n"[..]),
        ("README.md", &b"# tool\n"[..]),
    ] {
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file(name, opts).expect("zip entry");
        w.write_all(data).expect("zip data");
    }
    let mut bytes = w.finish().expect("zip").into_inner();
    // Walk the central directory from the end record: its entry count
    // (offset 10) and start (offset 16).
    let eocd = bytes
        .windows(4)
        .rposition(|s| s == b"PK\x05\x06")
        .expect("end of central directory");
    let u16_at = |b: &[u8], at: usize| usize::from(u16::from_le_bytes([b[at], b[at + 1]]));
    let count = u16_at(&bytes, eocd + 10);
    let mut at = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().expect("u32")) as usize;
    for _ in 0..count {
        assert_eq!(&bytes[at..at + 4], b"PK\x01\x02", "central record");
        bytes[at + 5] = 0;
        bytes[at + 38..at + 42].copy_from_slice(&0x20u32.to_le_bytes());
        at += 46 + u16_at(&bytes, at + 28) + u16_at(&bytes, at + 30) + u16_at(&bytes, at + 32);
    }
    bytes.leak()
}

/// The common shape of a release zip: a binary next to a README, built where
/// no Unix mode is recorded. `include` keeps only the binary,
/// `executable = True` makes it runnable, and the extract is a tool another
/// target runs directly.
#[tokio::test]
async fn release_asset_picks_the_binary_out_of_a_zip_and_makes_it_executable() -> anyhow::Result<()>
{
    let body = windows_zip();
    let (url, _hits) = serve(body, 4);
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
tool = heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.0",
    asset = "tool-linux.zip",
    sha256 = "{}",
    extract = True,
    include = "tool",
    executable = True,
    server_url = "{url}",
)

target(
    name = "use",
    driver = "bash",
    tools = [":tool"],
    run = "tool > $OUT",
    out = "out.txt",
)
"#,
            sha256(body)
        ),
    );
    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result),
        vec![(
            "p/tool/tool".to_string(),
            "#!/bin/sh\necho from-the-zip\n".to_string(),
            true
        )],
        "only the binary, executable"
    );
    let result = ws.run("//p:use").await?;
    assert_eq!(
        files(&result),
        vec![("p/out.txt".to_string(), "from-the-zip\n".to_string(), false)]
    );
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

/// One request [`serve_api`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiRequest {
    path: String,
    accept: String,
    authorization: String,
}

/// basic auth for `fetcher:netrc-e2e-token`, what [`write_credential`]'s
/// netrc entry presents.
const BASIC_AUTH: &str = "Basic ZmV0Y2hlcjpuZXRyYy1lMmUtdG9rZW4=";

/// A loopback server playing the GitHub API for the private repository `o/r`:
///
/// - `GET /repos/o/r/releases/tags/v1` answers the release JSON, listing
///   `other.txt` (id 6) and `tool.sh` (id 7), each with its API `url` on this
///   server, and `tool.sh`'s download count;
/// - `GET /repos/o/r/releases/assets/7` answers `asset`, and only to
///   `Accept: application/octet-stream` (406 otherwise; GitHub would answer the
///   asset's JSON metadata instead).
///
/// Every route answers 401 without [`BASIC_AUTH`], and anything else is a 404.
/// Returns the base URL and every request served, in order.
fn serve_api(asset: &'static [u8]) -> (String, Requests) {
    let (base, seen, _tool_id) = serve_api_live(asset, Some(7));
    (base, seen)
}

/// Every request a fake API served, in order.
type Requests = Arc<Mutex<Vec<ApiRequest>>>;

/// `tool.sh`'s id on a fake API, `None` while it is not uploaded.
type ToolId = Arc<Mutex<Option<u32>>>;

/// [`serve_api`], with `tool.sh`'s id under the test's control: the third
/// value is its id now, `None` while it is not uploaded. The JSON lists it
/// under that id (not at all under `None`), and only that id's URL serves it;
/// a former id answers 404, as GitHub does once an asset is replaced.
fn serve_api_live(asset: &'static [u8], tool_id: Option<u32>) -> (String, Requests, ToolId) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool_id = Arc::new(Mutex::new(tool_id));
    // As on GitHub, the JSON's bytes change between lookups: the download
    // count moves with every download.
    let api = base.clone();
    let release = move |downloads: usize, id: Option<u32>| {
        let tool = id.map_or_else(String::new, |id| {
            format!(
                r#", {{"name": "tool.sh", "url": "{api}/repos/o/r/releases/assets/{id}", "download_count": {downloads}}}"#
            )
        });
        format!(
            r#"{{"tag_name": "v1", "assets": [
            {{"name": "other.txt", "url": "{api}/repos/o/r/releases/assets/6", "download_count": 3}}{tool}
        ]}}"#
        )
    };

    let log = Arc::clone(&seen);
    let current = Arc::clone(&tool_id);
    std::thread::spawn(move || {
        let mut downloads = 0;
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let Ok(read_half) = sock.try_clone() else {
                return;
            };
            let mut lines = std::io::BufReader::new(read_half).lines();
            let request_line = lines.next().and_then(Result::ok).unwrap_or_default();
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let (mut accept, mut authorization) = (String::new(), String::new());
            for line in lines.map_while(Result::ok).take_while(|l| !l.is_empty()) {
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                if k.eq_ignore_ascii_case("accept") {
                    accept = v.trim().to_string();
                } else if k.eq_ignore_ascii_case("authorization") {
                    authorization = v.trim().to_string();
                }
            }
            let req = ApiRequest {
                path,
                accept,
                authorization,
            };
            let id = *current.lock().expect("tool id");
            let json;
            let (status, body): (&str, &[u8]) = if req.authorization != BASIC_AUTH {
                ("401 Unauthorized", b"Bad credentials")
            } else if req.path == RELEASE_PATH {
                json = release(downloads, id);
                ("200 OK", json.as_bytes())
            } else if id.is_some_and(|id| req.path == asset_path(id)) {
                if req.accept == "application/octet-stream" {
                    downloads += 1;
                    ("200 OK", asset)
                } else {
                    ("406 Not Acceptable", b"asset bytes need octet-stream")
                }
            } else {
                ("404 Not Found", b"Not Found")
            };
            log.lock().expect("request log").push(req);
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
        }
    });
    (base, seen, tool_id)
}

/// The API path of the asset with id `id`.
fn asset_path(id: u32) -> String {
    format!("/repos/o/r/releases/assets/{id}")
}

/// `//auth:files`: a credential presenting `fetcher:netrc-e2e-token` as a
/// netrc entry for the loopback host.
fn write_credential(ws: &htestkit::Workspace) {
    ws.write_file(
        "secrets/creds.json",
        r#"{"user": "fetcher", "token": "netrc-e2e-token"}"#,
    );
    let root = ws.dir.path().display().to_string();
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(
    name    = "files",
    driver  = "auth.credential",
    sources = [heph.auth.file("{root}/secrets/creds.json",
                              fields = {{"username": "user", "token": "token"}})],
    present = heph.auth.netrc(["127.0.0.1"]),
)
"#
        ),
    );
}

/// A workspace with [`write_credential`]'s credential, and the arguments that
/// point a `release_asset` call at the API server `api`.
fn private_workspace(api: &str) -> anyhow::Result<(htestkit::Workspace, String)> {
    let ws = workspace()?;
    write_credential(&ws);
    Ok((ws, format!(r#"server_url = "{api}", api_url = "{api}","#)))
}

fn paths(requests: &Mutex<Vec<ApiRequest>>) -> Vec<String> {
    requests
        .lock()
        .expect("request log")
        .iter()
        .map(|r| r.path.clone())
        .collect()
}

const RELEASE_PATH: &str = "/repos/o/r/releases/tags/v1";
const ASSET_PATH: &str = "/repos/o/r/releases/assets/7";

/// A private asset through the API: the release JSON is looked up with the
/// credential, the asset's URL is picked out of it and read by the asset
/// fetch as a deferred value, and the bytes arrive only because that fetch
/// sent the credential and `Accept: application/octet-stream` to the URL from
/// the JSON. A second build makes no request at all: the release JSON is
/// cached by its URL (`unpinned`, not `cache = False`), and so is the asset.
#[tokio::test]
async fn release_asset_with_credentials_fetches_a_private_asset() -> anyhow::Result<()> {
    const SCRIPT: &[u8] = b"#!/bin/sh\necho private\n";
    let (api, requests) = serve_api(SCRIPT);
    let (ws, at_api) = private_workspace(&api)?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "tool.sh",
    sha256 = "{}",
    executable = True,
    credentials = ["//auth:files"],
    {at_api}
)
"#,
            sha256(SCRIPT)
        ),
    );
    assert_eq!(
        ws.get_spec("//p:tool_url").await?.driver,
        "gh.asset_url",
        "the URL is resolved by a target"
    );
    assert_eq!(
        ws.get_spec("//p:tool").await?.config.get("url"),
        Some(&hcore::htvalue::Value::String(
            "${read://p:tool_url}".to_string()
        )),
        "the asset fetch reads that target's output"
    );

    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result),
        vec![(
            "p/tool.sh".to_string(),
            "#!/bin/sh\necho private\n".to_string(),
            true
        )]
    );
    let seen = requests.lock().expect("request log").clone();
    assert_eq!(
        seen,
        vec![
            ApiRequest {
                path: RELEASE_PATH.to_string(),
                accept: "application/vnd.github+json".to_string(),
                authorization: BASIC_AUTH.to_string(),
            },
            ApiRequest {
                path: ASSET_PATH.to_string(),
                accept: "application/octet-stream".to_string(),
                authorization: BASIC_AUTH.to_string(),
            },
        ]
    );

    ws.run("//p:tool").await?;
    assert_eq!(
        paths(&requests),
        [RELEASE_PATH, ASSET_PATH],
        "a warm build hits the cache for the release JSON and the asset"
    );
    Ok(())
}

/// The extract variant: the archive is fetched through the API and unpacked.
#[tokio::test]
async fn release_asset_with_credentials_extracts_a_private_archive() -> anyhow::Result<()> {
    let body = tarball();
    let (api, requests) = serve_api(body);
    let (ws, at_api) = private_workspace(&api)?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "tool.sh",
    format = "tar.gz",
    sha256 = "{}",
    extract = True,
    strip_components = 1,
    credentials = ["//auth:files"],
    {at_api}
)
"#,
            sha256(body)
        ),
    );
    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result)
            .into_iter()
            .map(|(path, _, x)| (path, x))
            .collect::<Vec<_>>(),
        vec![
            ("p/tool/README".to_string(), false),
            ("p/tool/bin/hello".to_string(), true),
        ]
    );
    assert_eq!(paths(&requests), [RELEASE_PATH, ASSET_PATH]);
    Ok(())
}

/// A gzipped tarball holding one file at `path`, written straight into the
/// header so `tar`'s builder cannot refuse it, as an attacker's archive is
/// not built with a careful builder.
fn raw_tarball(path: &str) -> &'static [u8] {
    let data = b"pwned\n";
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    let name = &mut h.as_old_mut().name;
    name.fill(0);
    name.get_mut(..path.len())
        .expect("path fits the header")
        .copy_from_slice(path.as_bytes());
    h.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&h, &data[..]).expect("append");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&b.into_inner().expect("tar")).expect("gzip");
    gz.finish().expect("gzip").leak()
}

/// Each `CacheOutcome` of `addr` over one request building it.
async fn outcomes(
    ws: &htestkit::Workspace,
    addr: &str,
) -> anyhow::Result<Vec<hcore::events::CacheOutcomeKind>> {
    let m = heph::htmatcher::Matcher::Addr(heph::htaddr::parse_addr(addr)?);
    let (res, events) = crate::common::run_drain_events(&ws.engine, &m).await;
    let batch = res?;
    assert!(batch.errors.is_empty(), "{addr}: {:?}", batch.errors);
    Ok(events
        .iter()
        .filter_map(|e| match &e.kind {
            heph::engine::event::BuildEventKind::CacheOutcome {
                addr: a, outcome, ..
            } if a == addr => Some(*outcome),
            _ => None,
        })
        .collect())
}

/// An archive with an entry climbing out of the output (`../evil`) fails the
/// extract, naming the target and the entry; nothing escapes, and nothing is
/// cached: the next build refuses it again (from the cached archive).
#[tokio::test]
async fn release_asset_refuses_an_archive_escaping_its_output() -> anyhow::Result<()> {
    let body = raw_tarball("../evil");
    let (url, hits) = serve(body, 2);
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
    sha256 = "{}",
    extract = True,
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );
    for attempt in 0..2 {
        let Err(err) = ws.run("//p:tool").await else {
            panic!("attempt {attempt}: an escaping entry must fail the build");
        };
        let msg = format!("{err:#}");
        for want in [
            "//p:tool",
            r#"tar entry "../evil""#,
            "`..` path component refused",
        ] {
            assert!(msg.contains(want), "attempt {attempt}: {want}: {msg}");
        }
    }
    assert!(!ws.dir.path().join("p/evil").exists());
    assert!(!ws.dir.path().join("evil").exists());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the archive itself is cached"
    );
    Ok(())
}

/// An `include` pattern that matches nothing is no error of the
/// `release_asset` call, which only declares targets: the BUILD file
/// evaluates and the fetch builds. The declared extract fails at run, naming
/// itself and the pattern, and listing what the pattern was matched against.
#[tokio::test]
async fn release_asset_with_an_unmatched_include_fails_the_extract() -> anyhow::Result<()> {
    let body = tarball();
    let (url, _hits) = serve(body, 4);
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
    sha256 = "{}",
    extract = True,
    strip_components = 1,
    include = ["bin/hello", "bin/helo"],
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );
    assert_eq!(ws.get_spec("//p:tool").await?.driver, "archive.extract");
    ws.run("//p:tool_archive").await?;
    let Err(err) = ws.run("//p:tool").await else {
        panic!("an unmatched `include` pattern must fail the extract");
    };
    let msg = format!("{err:#}");
    for want in [
        "//p:tool",
        r#"`include` pattern "bin/helo" matches no file or link in the archive"#,
        r#"["bin/hello", "README"]"#,
    ] {
        assert!(msg.contains(want), "{want}: {msg}");
    }
    Ok(())
}

/// A nested `out` puts the tree there; the extract is cached, so the next
/// build is a hit and does not unpack again.
#[tokio::test]
async fn release_asset_extracts_into_a_nested_out_and_is_cached() -> anyhow::Result<()> {
    let body = tarball();
    let (url, hits) = serve(body, 2);
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
    sha256 = "{}",
    extract = True,
    strip_components = 1,
    out = "vendor/tool",
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );
    let result = ws.run("//p:tool").await?;
    assert_eq!(
        files(&result)
            .into_iter()
            .map(|(path, _, x)| (path, x))
            .collect::<Vec<_>>(),
        vec![
            ("p/vendor/tool/README".to_string(), false),
            ("p/vendor/tool/bin/hello".to_string(), true),
        ]
    );
    assert_eq!(
        outcomes(&ws, "//p:tool").await?,
        [hcore::events::CacheOutcomeKind::LocalHit],
        "the extracted tree is served from the cache"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    Ok(())
}

/// `max_entries` passed to the call reaches the extract driver through the
/// declaration: a two-entry tarball over a limit of one fails, naming it.
#[tokio::test]
async fn release_asset_forwards_the_unpack_limits() -> anyhow::Result<()> {
    let body = tarball();
    let (url, _hits) = serve(body, 1);
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
    sha256 = "{}",
    extract = True,
    max_entries = 1,
    server_url = "{url}",
)
"#,
            sha256(body)
        ),
    );
    let Err(err) = ws.run("//p:tool").await else {
        panic!("a tarball over `max_entries` must fail the build");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("more than 1 entries; raise `max_entries`"),
        "got: {msg}"
    );
    Ok(())
}

/// An asset the release does not have fails at `<name>_url`, listing the
/// names it does have, and nothing is fetched from an asset URL.
#[tokio::test]
async fn release_asset_with_credentials_and_a_wrong_name_lists_the_assets() -> anyhow::Result<()> {
    let (api, requests) = serve_api(b"unused");
    let (ws, at_api) = private_workspace(&api)?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "missing.sh",
    credentials = ["//auth:files"],
    {at_api}
)
"#
        ),
    );
    let Err(err) = ws.run("//p:tool").await else {
        panic!("an asset the release does not list must fail the build");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains(
            r#"the release has no asset named "missing.sh"; it has ["other.txt", "tool.sh"]"#
        ),
        "got: {msg}"
    );
    assert_eq!(paths(&requests), [RELEASE_PATH]);
    Ok(())
}

const STALE_SCRIPT: &[u8] = b"#!/bin/sh\necho stale\n";

/// A workspace over [`serve_api_live`] whose `//p:tool` is the private
/// `tool.sh`, pinned to [`STALE_SCRIPT`].
fn stale_workspace(
    tool_id: Option<u32>,
) -> anyhow::Result<(htestkit::Workspace, Requests, ToolId)> {
    let (api, requests, id) = serve_api_live(STALE_SCRIPT, tool_id);
    let (ws, at_api) = private_workspace(&api)?;
    ws.write_build_file(
        "p",
        &format!(
            r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "tool.sh",
    sha256 = "{}",
    credentials = ["//auth:files"],
    {at_api}
)
"#,
            sha256(STALE_SCRIPT)
        ),
    );
    Ok((ws, requests, id))
}

/// `heph run --force <addr>`.
async fn run_forced(ws: &htestkit::Workspace, addr: &str) -> anyhow::Result<()> {
    let m = heph::htmatcher::Matcher::Addr(heph::htaddr::parse_addr(addr)?);
    let forced = heph::engine::ResultOptions {
        force: true,
        ..Default::default()
    };
    ws.run_matcher_with(&m, &forced).await?;
    Ok(())
}

/// The cached release JSON names an asset id the server no longer has (the
/// asset was uploaded again): the asset fetch 404s, and the error names the
/// target the URL came from and the exact `heph run --force` command for the
/// JSON's target, without the JSON being looked up again. Forcing
/// `<name>_release` refetches it, and the next build fetches the new id.
#[tokio::test]
async fn release_asset_with_a_stale_cached_release_json_says_how_to_refresh_it()
-> anyhow::Result<()> {
    let (ws, requests, tool_id) = stale_workspace(Some(7))?;
    // The JSON is cached; the asset is not (not built yet, or evicted).
    ws.run("//p:tool_release").await?;
    *tool_id.lock().expect("tool id") = Some(8);

    let Err(err) = ws.run("//p:tool").await else {
        panic!("the asset id the cached JSON names is gone");
    };
    let msg = format!("{err:#}");
    for want in [
        "404",
        "the url is the output of //p:tool_url",
        "`heph run --force //p:tool_release`",
    ] {
        assert!(msg.contains(want), "{want}: {msg}");
    }
    assert!(!msg.contains("--force //p:tool_url"), "uncached: {msg}");
    assert_eq!(
        paths(&requests),
        [RELEASE_PATH, asset_path(7).as_str()],
        "the release JSON was served from the cache"
    );

    run_forced(&ws, "//p:tool_release").await?;
    let result = ws.run("//p:tool").await?;
    assert_eq!(files(&result)[0].1, "#!/bin/sh\necho stale\n");
    assert_eq!(
        paths(&requests)[2..],
        [RELEASE_PATH, asset_path(8).as_str()],
        "the forced run looked the release up again, and the asset came from the new id"
    );
    Ok(())
}

/// A release JSON cached before the asset was uploaded lacks it: `<name>_url`
/// fails with the exact `heph run --force` command for the JSON's target, and
/// keeps failing from the cache once the asset is up. Forcing the JSON
/// recovers.
#[tokio::test]
async fn release_asset_cached_before_the_upload_says_how_to_refresh_it() -> anyhow::Result<()> {
    let (ws, requests, tool_id) = stale_workspace(None)?;
    for attempt in 0..2 {
        let Err(err) = ws.run("//p:tool").await else {
            panic!("attempt {attempt}: the cached JSON lists no tool.sh");
        };
        let msg = format!("{err:#}");
        for want in [
            r#"the release has no asset named "tool.sh""#,
            "`heph run --force //p:tool_release`",
        ] {
            assert!(msg.contains(want), "attempt {attempt}: {want}: {msg}");
        }
        // Uploaded after the first build looked the release up.
        *tool_id.lock().expect("tool id") = Some(7);
    }
    assert_eq!(paths(&requests), [RELEASE_PATH], "the JSON is cached");

    run_forced(&ws, "//p:tool_release").await?;
    ws.run("//p:tool").await?;
    assert_eq!(
        paths(&requests),
        [RELEASE_PATH, RELEASE_PATH, ASSET_PATH],
        "recovered by the forced lookup"
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

/// So is the API, for a private asset: two calls differing only by `api_url`
/// declare `asset_url` targets with different def hashes (the API the asset's
/// URL must be under is in its config), and a trailing `/` is the same API.
#[tokio::test]
async fn api_url_reaches_the_asset_url_def_hash() -> anyhow::Result<()> {
    const CREDS: &str = r#"credentials = ["//auth:gh"],"#;
    let api = |url: &str| format!(r#"{CREDS} api_url = "{url}","#);
    let one = def_hash(&api("https://api.ghe.example"), "tool_url").await?;
    assert_eq!(
        one,
        def_hash(&api("https://api.ghe.example/"), "tool_url").await?,
        "a trailing `/` is the same API"
    );
    assert_ne!(one, def_hash(CREDS, "tool_url").await?);
    assert_ne!(
        one,
        def_hash(&api("https://api2.ghe.example"), "tool_url").await?
    );
    Ok(())
}

/// T8: the per-platform example in `docs/MATRIX.md` (the block after its
/// marker) evaluates through the real `release_asset`: three cells named
/// `tool.<os>-<arch>`, each an `archive.extract` over its own fetch, and the
/// matrix's cell values (in cell order) and `default` being the full addresses
/// `release_asset` returned.
#[tokio::test]
async fn gh_release_matrix_example() -> anyhow::Result<()> {
    const DOC: &str = include_str!("../../../../docs/MATRIX.md");
    let (_, after) = DOC
        .split_once("gh_release_matrix_example evaluates the block below")
        .expect("the marker is in docs/MATRIX.md");
    let block = after
        .split_once("```python\n")
        .and_then(|(_, rest)| rest.split_once("```"))
        .map(|(block, _)| block)
        .expect("a python block follows the marker");
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            "{}\ntarget(name = \"probe\", driver = \"http_fetch\", url = \"https://x\", cells = [c.value for c in TOOL.cells], default = TOOL.default)\n",
            block.replace("<sha256>", &"0".repeat(64))
        ),
    );

    let names = ["tool.linux-amd64", "tool.linux-arm64", "tool.darwin-arm64"];
    for n in names {
        assert_eq!(
            ws.get_spec(&format!("//p:{n}")).await?.driver,
            "archive.extract"
        );
        assert_eq!(
            ws.get_spec(&format!("//p:{n}_archive")).await?.driver,
            "http_fetch"
        );
    }
    let probe = ws.get_spec("//p:probe").await?;
    let want_cells = names
        .iter()
        .map(|n| hcore::htvalue::Value::String(format!("//p:{n}")))
        .collect();
    assert_eq!(
        probe.config.get("cells"),
        Some(&hcore::htvalue::Value::List(want_cells)),
        "each cell's value is the address release_asset returned, in axis order"
    );
    assert_eq!(
        probe.config.get("default"),
        Some(&hcore::htvalue::Value::String(format!(
            "//p:tool.{}-{}",
            hcore::htplatform::os(),
            hcore::htplatform::arch()
        ))),
        "the host's cell, on every supported host"
    );
    Ok(())
}
