//! `go.headers`: the bytes `go/build` reads of each file `go list` opens, as one
//! tar.
//!
//! `go list` opens a C/asm-family file in the package directory only to read its
//! leading comment block (`go/build.readComments`), where the build constraints
//! live; and a `.go` file only for its go/build view (`readGoInfo`: the header
//! through the imports, plus the `//go:embed` comments of a file importing
//! `"embed"`, see [`crate::plugingo::go_view`]). This driver extracts exactly
//! that from every file of [`crate::plugingo::nongo_lanes::header_read_glob_addr`]
//! and [`crate::plugingo::nongo_lanes::go_src_glob_addr`] and writes them, one
//! entry per file under its ws-relative path, into a deterministic tar.
//! `_golist` depends on it `hashed: true, runtime: false`: editing the body of a
//! `.c` below its header, or a function body of a `.go` file, leaves this
//! output — and so `_golist`'s key — unchanged, while a new constraint or
//! import, an added file or a rename moves it.
//!
//! Cached locally only. Its output is a pure function of its input, and cheap to
//! recompute, so the remote cache would only carry bytes back and forth.

use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Read};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// Bump whenever the extractor or the tar layout changes: it is in the def hash,
/// so every cached header tar is recomputed rather than served in the old shape.
/// v2: `.go` files joined the tar, as their go/build view.
pub const GO_HEADERS_FORMAT_VERSION: u32 = 2;

/// How much of a file is read per `read` call. The extractor stops at the end of
/// the prefix, so it reads at most the prefix plus one chunk of any file.
const READ_CHUNK: usize = 4096;

/// Origin id of the header-read glob input.
const SRC_ORIGIN: &str = "dep|src|0";
/// Origin id of the `.go` glob input.
const GO_SRC_ORIGIN: &str = "dep|gosrc|0";

/// The UTF-8 byte-order mark `go/build` skips at the start of a file.
const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

/// The prefix of a C/asm-family file that `go/build` reads: the bytes
/// `readComments` (`$GOROOT/src/go/build/read.go`) consumes, verbatim — a
/// leading UTF-8 BOM, then whitespace, `;`, `//` and `/* */` comments, up to and
/// including the first byte that ends them (the first code byte, the byte after
/// a stray `/`, a NUL and the byte after it). Everything `go/build` concludes
/// about the file (its build constraints, and whether reading it failed) is a
/// function of these bytes, so hashing them is hashing what `go list` saw.
///
/// Streams: reads at most the returned length plus one [`READ_CHUNK`], however
/// large the file. An I/O error is returned, not folded into the prefix.
pub fn extract_go_build_header(r: impl Read) -> std::io::Result<Vec<u8>> {
    extract_with_chunk(r, READ_CHUNK)
}

fn extract_with_chunk(r: impl Read, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut src = std::io::BufReader::with_capacity(chunk, r);
    let mut out = Vec::new();
    let (bom, pending) = take_bom(&mut src)?;
    if bom {
        out.extend_from_slice(&BOM);
    }
    scan(&mut src, out, pending)
}

/// `newImportReader`'s BOM step: a leading UTF-8 BOM is discarded before
/// scanning (it is still part of what was read, so callers keep it in their
/// output). Returns whether there was one, and the bytes read ahead that turned
/// out not to be one — the start of the scan.
pub(crate) fn take_bom(src: &mut impl BufRead) -> std::io::Result<(bool, Vec<u8>)> {
    let lead = src.fill_buf()?;
    if lead.starts_with(&BOM) {
        src.consume(BOM.len());
        return Ok((true, Vec::new()));
    }
    if lead.len() < BOM.len() && BOM.starts_with(lead) && !lead.is_empty() {
        // A short read that could still be a BOM: settle it byte by byte.
        let mut peeked = Vec::with_capacity(BOM.len());
        while peeked.len() < BOM.len() {
            let Some(b) = next_byte(src)? else { break };
            peeked.push(b);
            if !BOM.starts_with(&peeked) {
                break;
            }
        }
        if peeked == BOM {
            return Ok((true, Vec::new()));
        }
        return Ok((false, peeked));
    }
    Ok((false, Vec::new()))
}

/// One byte from `src`, `None` at EOF.
pub(crate) fn next_byte(src: &mut impl BufRead) -> std::io::Result<Option<u8>> {
    let buf = loop {
        match src.fill_buf() {
            Ok(b) => break b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    };
    let Some(&b) = buf.first() else {
        return Ok(None);
    };
    src.consume(1);
    Ok(Some(b))
}

/// `importReader` reduced to what `readComments` uses. `err` folds Go's
/// `errSyntax` and `errNUL`: both stop the scan, and which one it was is
/// recoverable from the bytes.
struct Scanner<'a, R> {
    src: &'a mut R,
    /// Bytes read ahead of `src` (a would-be BOM that was not one).
    pending: std::vec::IntoIter<u8>,
    buf: Vec<u8>,
    err: bool,
    eof: bool,
}

impl<R: BufRead> Scanner<'_, R> {
    /// `importReader.readByte`: read one byte, record it, flag NUL and EOF.
    /// Reads even after `err` is set, exactly as Go's does.
    fn read_byte(&mut self) -> std::io::Result<u8> {
        let next = match self.pending.next() {
            Some(b) => Some(b),
            None => next_byte(self.src)?,
        };
        match next {
            Some(c) => {
                self.buf.push(c);
                if c == 0 {
                    self.err = true;
                }
                Ok(c)
            }
            None => {
                self.eof = true;
                Ok(0)
            }
        }
    }
}

/// `readComments`: `peekByte(true)` once, keeping every byte it read.
fn scan(src: &mut impl BufRead, mut out: Vec<u8>, pending: Vec<u8>) -> std::io::Result<Vec<u8>> {
    let mut r = Scanner {
        src,
        pending: pending.into_iter(),
        buf: Vec::new(),
        err: false,
        eof: false,
    };
    let mut c = r.read_byte()?;
    while !r.err && !r.eof {
        match c {
            b' ' | b'\x0c' | b'\t' | b'\r' | b'\n' | b';' => {
                c = r.read_byte()?;
                continue;
            }
            b'/' => {
                c = r.read_byte()?;
                if c == b'/' {
                    while c != b'\n' && !r.err && !r.eof {
                        c = r.read_byte()?;
                    }
                } else if c == b'*' {
                    let mut c1 = 0u8;
                    while (c != b'*' || c1 != b'/') && !r.err {
                        if r.eof {
                            r.err = true;
                        }
                        c = c1;
                        c1 = r.read_byte()?;
                    }
                } else {
                    r.err = true;
                }
                c = r.read_byte()?;
                continue;
            }
            _ => {}
        }
        break;
    }
    out.append(&mut r.buf);
    Ok(out)
}

/// One tar entry per file, sorted by path: the [`extract_go_build_header`] of
/// `ws_dir/<rel>` under `rel`, or for a `.go` file its
/// [`crate::plugingo::go_view::extract_go_view`]. Mode, owner and mtime are fixed, so equal inputs
/// give byte-equal tars on every host. A file with an empty prefix still gets
/// its (empty) entry: adding or removing one must move the key.
///
/// A file that cannot be read fails the whole target, naming it: silently
/// leaving it out would hand `_golist` a key that misses it. `cancel` is
/// checked before each file, so a cancelled build stops between files.
pub fn build_headers_tar(
    ws_dir: &std::path::Path,
    rels: &[String],
    cancel: &dyn Cancellable,
) -> anyhow::Result<Vec<u8>> {
    let mut sorted: Vec<&String> = rels.iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut builder = tar::Builder::new(Vec::new());
    for rel in sorted {
        anyhow::ensure!(!cancel.is_cancelled(), "go.headers: cancelled");
        let path = ws_dir.join(rel);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("go.headers: open header-read file {rel}"))?;
        let header_bytes = if rel.ends_with(".go") {
            crate::plugingo::go_view::extract_go_view(file)
        } else {
            extract_go_build_header(file)
        }
        .with_context(|| format!("go.headers: read the go/build header of {rel}"))?;
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(header_bytes.len() as u64);
        builder
            .append_data(&mut h, rel, header_bytes.as_slice())
            .with_context(|| format!("go.headers: add {rel} to the header tar"))?;
    }
    builder
        .into_inner()
        .context("go.headers: finish the header tar")
}

/// The ws-relative paths listed in a staged input's list file.
fn staged_rels(
    list_path: &std::path::Path,
    unpack_root: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let listed = std::fs::read_to_string(list_path)
        .with_context(|| format!("read staged file list {list_path:?}"))?;
    listed
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let rel = std::path::Path::new(line)
                .strip_prefix(unpack_root)
                .with_context(|| format!("staged file {line} is outside {unpack_root:?}"))?;
            let rel = rel
                .to_str()
                .with_context(|| format!("staged path is not UTF-8: {}", rel.display()))?;
            Ok(rel.to_string())
        })
        .collect()
}

/// Config of a `go.headers` target (provider-generated, see
/// [`crate::plugingo::nongo_lanes::headers_spec`]).
#[derive(Spec)]
struct GoHeadersSpec {
    /// The header-read content glob whose files are extracted.
    #[spec(required)]
    src: String,
    /// The package's `.go` glob, whose files' go/build views are extracted.
    #[spec(required)]
    go_src: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GoHeadersDef {
    src: String,
    go_src: String,
}

impl Hash for GoHeadersDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.src.hash(state);
        self.go_src.hash(state);
    }
}

/// The def hash of a header target: the extractor format `version` (always
/// [`GO_HEADERS_FORMAT_VERSION`] outside tests), then the def.
fn def_hash(addr: &hmodel::htaddr::Addr, def: &GoHeadersDef, version: u32) -> Vec<u8> {
    let mut h = DebugHasher::new(Xxh3Default::new(), || {
        format!("go_headers_{}", addr.format())
    });
    version.hash(&mut h);
    def.hash(&mut h);
    format!("{:x}", h.finish()).into_bytes()
}

pub struct GoHeadersDriver;

#[async_trait]
impl ManagedDriver for GoHeadersDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "headers".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GoHeadersSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let spec =
            GoHeadersSpec::from(&req.target_spec.config).context("parse go.headers config")?;
        let input = |addr: &str, origin: &str| {
            anyhow::Ok(Input {
                r#ref: TargetAddr::parse(addr, &pkg)
                    .with_context(|| format!("parse go.headers input {addr}"))?,
                mode: InputMode::Standard,
                origin_id: origin.to_string(),
                annotations: std::collections::BTreeMap::new(),
                hashed: true,
                runtime: true,
            })
        };
        let inputs = vec![
            input(&spec.src, SRC_ORIGIN)?,
            input(&spec.go_src, GO_SRC_ORIGIN)?,
        ];
        let def = GoHeadersDef {
            src: spec.src,
            go_src: spec.go_src,
        };
        let hash = def_hash(&req.target_spec.addr, &def, GO_HEADERS_FORMAT_VERSION);
        let out_path = if pkg.as_str().is_empty() {
            crate::plugingo::nongo_lanes::HEADERS_OUT.to_string()
        } else {
            format!(
                "{}/{}",
                pkg.as_str(),
                crate::plugingo::nongo_lanes::HEADERS_OUT
            )
        };
        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out_path),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                // Local only: see the module docs.
                cache: CacheConfig::on(false),
                hash,
                ..Default::default()
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        // Every artifact of a glob is its own input, and all of them append to
        // one shared list file per origin — so the first input's list is the
        // whole list of that glob (as `staged_path` reads it). A glob matching
        // no file stages nothing; with neither, the tar is empty.
        let staged: Vec<_> = [SRC_ORIGIN, GO_SRC_ORIGIN]
            .iter()
            .filter_map(|origin| req.inputs.iter().find(|m| m.input.origin_id == *origin))
            .map(|m| anyhow::Ok((m.require_list_path()?.to_path_buf(), m.unpack_root.clone())))
            .collect::<anyhow::Result<_>>()?;
        let cancel = ctoken.clone_arc();
        let ws_dir = req.sandbox_ws_dir.clone();
        let pkg_dir = req.sandbox_pkg_dir.clone();
        // The list read, every header read and the write are file IO: the
        // bounded blocking pool, never a runtime worker.
        hcore::blocking::run(move || {
            let mut rels = Vec::new();
            for (list_path, unpack_root) in &staged {
                rels.extend(staged_rels(list_path, unpack_root)?);
            }
            let tar = build_headers_tar(&ws_dir, &rels, &*cancel)?;
            std::fs::create_dir_all(&pkg_dir).with_context(|| format!("create {pkg_dir:?}"))?;
            let out = pkg_dir.join(crate::plugingo::nongo_lanes::HEADERS_OUT);
            std::fs::write(&out, tar).with_context(|| format!("write {out:?}"))?;
            anyhow::Ok(())
        })
        .await?;
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::Value;
    use std::collections::HashMap;

    fn extract(input: &[u8]) -> Vec<u8> {
        extract_go_build_header(input).expect("in-memory read cannot fail")
    }

    /// [`super::build_headers_tar`] for a build that is never cancelled. Shadows
    /// the glob import, so the tests below read as calls of the real thing.
    fn build_headers_tar(ws_dir: &std::path::Path, rels: &[String]) -> anyhow::Result<Vec<u8>> {
        super::build_headers_tar(ws_dir, rels, &hcore::hasync::StdCancellationToken::new())
    }

    /// T13: the extractor on the adversarial table. The expected value is what
    /// `readComments` consumes: the header, plus the byte that ended it.
    #[test]
    fn extractor_adversarial_table() {
        let cases: &[(&str, &[u8], &[u8])] = &[
            ("empty file", b"", b""),
            ("no comments", b"int x;\n", b"i"),
            (
                "go:build, blank line, code",
                b"//go:build linux\n\nint x;\n",
                b"//go:build linux\n\ni",
            ),
            (
                "+build with the blank line",
                b"// +build linux\n\nint x;\n",
                b"// +build linux\n\ni",
            ),
            (
                "+build without the blank line",
                b"// +build linux\nint x;\n",
                b"// +build linux\ni",
            ),
            (
                "multi-line block comment",
                b"/* a\n * b\n */\n#include <x.h>\n",
                b"/* a\n * b\n */\n#",
            ),
            (
                "unclosed block comment",
                b"/* never\nclosed",
                b"/* never\nclosed",
            ),
            ("/* inside a // line", b"// a /* b\nint", b"// a /* b\ni"),
            ("*/ then code on the line", b"/* a */ int x;", b"/* a */ i"),
            ("/*/ is not a close", b"/*/ x */y", b"/*/ x */y"),
            ("// at EOF, no newline", b"// x", b"// x"),
            ("CRLF", b"// a\r\n\r\nint", b"// a\r\n\r\ni"),
            ("lone CR inside a // line", b"// a\rb\nc d", b"// a\rb\nc"),
            ("lone CR as space", b"\r\rx", b"\r\rx"),
            (
                "UTF-8 BOM",
                b"\xef\xbb\xbf//go:build ignore\nint",
                b"\xef\xbb\xbf//go:build ignore\ni",
            ),
            ("a BOM-like prefix that is not one", b"\xef\xbbx", b"\xef"),
            (
                "invalid UTF-8 in a comment",
                b"// \xff\xfe\nint",
                b"// \xff\xfe\ni",
            ),
            // NUL stops the scan; Go still reads (and keeps) the byte after it.
            ("NUL in a // comment", b"// a\0bc\nint", b"// a\0b"),
            ("NUL as the first byte", b"\0abc", b"\0"),
            (
                "leading blank lines",
                b"\n\n  \t// a\nint",
                b"\n\n  \t// a\ni",
            ),
            (
                "a constraint after the first code line is dropped",
                b"int x;\n//go:build linux\n",
                b"i",
            ),
            ("semicolons are spaces", b";;//a\nb", b";;//a\nb"),
            // A stray `/`: a syntax error, after reading one more byte.
            ("stray slash", b"/xyz", b"/xy"),
            ("stray slash at EOF", b"/", b"/"),
        ];
        for (what, input, want) in cases {
            assert_eq!(
                extract(input),
                want.to_vec(),
                "{what}: {:?}",
                String::from_utf8_lossy(input)
            );
        }

        // What the extractor returns on this corpus, pinned next to the format
        // version that keys every cached header tar. A change to the extractor
        // that moves this digest must bump `GO_HEADERS_FORMAT_VERSION` (and
        // update both here); otherwise the old-shape tars are still served.
        let mut h = xxhash_rust::xxh3::Xxh3Default::new();
        for (_, input, _) in cases {
            let out = extract(input);
            h.update(&(out.len() as u64).to_le_bytes());
            h.update(&out);
        }
        assert_eq!(
            (GO_HEADERS_FORMAT_VERSION, format!("{:016x}", h.digest())),
            (2, "b7b582633246510f".to_string()),
            "the extractor's output moved: bump GO_HEADERS_FORMAT_VERSION and re-pin"
        );
    }

    /// The table again with a one-byte chunk, so every byte is a read boundary
    /// (the BOM included): the result never depends on where a read ends.
    #[test]
    fn extractor_is_independent_of_read_boundaries() {
        let inputs: &[&[u8]] = &[
            b"\xef\xbb\xbf//go:build ignore\nint",
            b"\xef\xbbx",
            b"/* a\n */\n#x",
            b"// a\0bc",
            b"/xyz",
        ];
        for input in inputs {
            for chunk in [1, 2, 3, 5, READ_CHUNK] {
                assert_eq!(
                    extract_with_chunk(*input, chunk).expect("read"),
                    extract(input),
                    "chunk {chunk}: {input:?}"
                );
            }
        }
    }

    /// A header that ends exactly on the read-chunk boundary, and one byte to
    /// either side of it.
    #[test]
    fn extractor_header_on_the_chunk_boundary() {
        for len in [READ_CHUNK - 1, READ_CHUNK, READ_CHUNK + 1] {
            // `// ` + filler + `\n`, `len` bytes in all.
            let mut header = b"// ".to_vec();
            header.resize(len - 1, b'x');
            header.push(b'\n');
            assert_eq!(header.len(), len);
            let mut file = header.clone();
            file.extend_from_slice(b"int x;\n");
            let mut want = header;
            want.push(b'i');
            assert_eq!(extract(&file), want, "header of {len} bytes");
        }
    }

    /// Counts the bytes the extractor pulls from its reader.
    struct Counting<R> {
        inner: R,
        read: usize,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    /// A 10 MB file with a one-line header: the extractor reads the header and
    /// at most one chunk past it — never the file.
    #[test]
    fn extractor_reads_only_the_prefix_of_a_large_file() {
        let header = b"//go:build linux\n";
        let mut file = header.to_vec();
        while file.len() < 10 * 1024 * 1024 {
            file.extend_from_slice(b"int x; /* body */\n");
        }
        let mut src = Counting {
            inner: file.as_slice(),
            read: 0,
        };
        let got = extract_go_build_header(&mut src).expect("read");
        let mut want = header.to_vec();
        want.push(b'i');
        assert_eq!(got, want);
        assert!(
            src.read <= header.len() + 1 + READ_CHUNK,
            "read {} bytes of a {}-byte file",
            src.read,
            file.len()
        );
    }

    fn tar_entries(tar: &[u8]) -> Vec<(String, Vec<u8>, tar::Header)> {
        let mut archive = tar::Archive::new(tar);
        archive
            .entries()
            .expect("entries")
            .map(|e| {
                let mut e = e.expect("entry");
                let path = e.path().expect("path").to_string_lossy().into_owned();
                let mut data = Vec::new();
                e.read_to_end(&mut data).expect("data");
                (path, data, e.header().clone())
            })
            .collect()
    }

    /// B3: one entry per file, sorted, empty headers included, fixed metadata —
    /// and the same bytes whatever order the files were staged in.
    #[test]
    fn header_tar_is_deterministic_and_complete() {
        let ws = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(ws.path().join("pkg")).expect("mkdir");
        std::fs::write(ws.path().join("pkg/b.s"), b"//go:build amd64\nTEXT x").expect("write");
        std::fs::write(ws.path().join("pkg/a.c"), b"int x;").expect("write");
        std::fs::write(ws.path().join("pkg/e.h"), b"").expect("write");
        let rels = vec![
            "pkg/b.s".to_string(),
            "pkg/e.h".to_string(),
            "pkg/a.c".to_string(),
        ];
        let tar = build_headers_tar(ws.path(), &rels).expect("tar");
        let mut reversed = rels.clone();
        reversed.reverse();
        assert_eq!(tar, build_headers_tar(ws.path(), &reversed).expect("tar"));

        let entries = tar_entries(&tar);
        let names: Vec<&str> = entries.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(names, ["pkg/a.c", "pkg/b.s", "pkg/e.h"]);
        assert_eq!(entries[0].1, b"i");
        assert_eq!(entries[1].1, b"//go:build amd64\nT");
        assert!(
            entries[2].1.is_empty(),
            "an empty header still has its entry"
        );
        for (_, _, h) in &entries {
            assert_eq!(h.mtime().expect("mtime"), 0);
            assert_eq!(h.uid().expect("uid"), 0);
            assert_eq!(h.gid().expect("gid"), 0);
            assert_eq!(h.mode().expect("mode"), 0o644);
        }
    }

    /// B3: removing the last `.c`, or one of two `.s` with identical headers,
    /// changes the tar — the key cannot miss a file going away.
    #[test]
    fn removing_a_file_changes_the_tar() {
        let ws = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(ws.path().join("pkg")).expect("mkdir");
        for f in ["a.s", "b.s", "only.c"] {
            std::fs::write(ws.path().join("pkg").join(f), b"//go:build amd64\nX").expect("write");
        }
        let all = ["pkg/a.s", "pkg/b.s", "pkg/only.c"].map(String::from);
        let full = build_headers_tar(ws.path(), &all).expect("tar");
        let without_c = build_headers_tar(ws.path(), &all[..2]).expect("tar");
        let without_b =
            build_headers_tar(ws.path(), &[all[0].clone(), all[2].clone()]).expect("tar");
        assert_ne!(full, without_c, "removing the last .c must move the key");
        assert_ne!(
            full, without_b,
            "removing one identical-header .s must move the key"
        );
        assert_ne!(
            build_headers_tar(ws.path(), &[]).expect("tar"),
            build_headers_tar(ws.path(), &all[2..]).expect("tar"),
        );
    }

    /// B3: a rename with the same bytes changes the tar (`x.c` → `x_windows.c`).
    #[test]
    fn renaming_a_file_changes_the_tar() {
        let ws = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(ws.path().join("pkg")).expect("mkdir");
        std::fs::write(ws.path().join("pkg/x.c"), b"int").expect("write");
        std::fs::write(ws.path().join("pkg/x_windows.c"), b"int").expect("write");
        assert_ne!(
            build_headers_tar(ws.path(), &["pkg/x.c".to_string()]).expect("tar"),
            build_headers_tar(ws.path(), &["pkg/x_windows.c".to_string()]).expect("tar"),
        );
    }

    /// T17: a staged file that cannot be read fails the target, naming it.
    #[test]
    fn an_unreadable_file_fails_loudly() {
        let ws = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(ws.path().join("pkg/dir.c")).expect("mkdir");
        for rel in ["pkg/missing.c", "pkg/dir.c"] {
            let err = build_headers_tar(ws.path(), &[rel.to_string()])
                .expect_err("an unreadable file must fail the target");
            assert!(format!("{err:#}").contains(rel), "{err:#}");
        }
    }

    /// T14: the header target is cached, locally only, and its def hash covers
    /// the extractor version and its input glob.
    #[tokio::test]
    async fn header_target_is_cached_locally_only() {
        let addr = crate::plugingo::nongo_lanes::headers_addr("pkg");
        let spec = crate::plugingo::nongo_lanes::headers_spec(addr, "pkg");
        let resp = GoHeadersDriver
            .parse(
                ParseRequest {
                    request_id: String::new(),
                    target_spec: Arc::new(spec.clone()),
                },
                &hcore::hasync::StdCancellationToken::new(),
            )
            .await
            .expect("parse");
        let def = resp.target_def;
        assert!(def.cache.enabled, "cached");
        assert!(!def.cache.remote_enabled, "never remote");
        assert_eq!(def.inputs.len(), 2);
        assert!(def.inputs.iter().all(|i| i.hashed && i.runtime));
        assert_eq!(
            def.inputs[0].r#ref.r#ref,
            crate::plugingo::nongo_lanes::header_read_glob_addr("pkg")
        );
        assert_eq!(
            def.inputs[1].r#ref.r#ref,
            crate::plugingo::nongo_lanes::go_src_glob_addr("pkg")
        );

        let mut other = spec;
        other.config = HashMap::from([
            (
                "src".to_string(),
                Value::String(crate::plugingo::nongo_lanes::header_read_glob_addr("qq").format()),
            ),
            (
                "go_src".to_string(),
                Value::String(crate::plugingo::nongo_lanes::go_src_glob_addr("pkg").format()),
            ),
        ]);
        let other = GoHeadersDriver
            .parse(
                ParseRequest {
                    request_id: String::new(),
                    target_spec: Arc::new(other),
                },
                &hcore::hasync::StdCancellationToken::new(),
            )
            .await
            .expect("parse");
        assert_ne!(
            def.hash, other.target_def.hash,
            "the input glob keys the def"
        );

        // The parsed def is keyed on the current format version, and a bump
        // moves it.
        let raw = GoHeadersDef {
            src: crate::plugingo::nongo_lanes::header_read_glob_addr("pkg").format(),
            go_src: crate::plugingo::nongo_lanes::go_src_glob_addr("pkg").format(),
        };
        assert_eq!(
            def.hash,
            def_hash(&def.addr, &raw, GO_HEADERS_FORMAT_VERSION),
            "parse keys the def on GO_HEADERS_FORMAT_VERSION"
        );
        assert_ne!(
            def.hash,
            def_hash(&def.addr, &raw, GO_HEADERS_FORMAT_VERSION + 1),
            "bumping the extractor format version moves the def hash"
        );
    }

    /// The run's file IO stops at the next file once the build is cancelled.
    #[test]
    fn header_tar_stops_when_cancelled() {
        let ws = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(ws.path().join("pkg")).expect("mkdir");
        std::fs::write(ws.path().join("pkg/a.c"), b"int").expect("write");
        let ct = hcore::hasync::StdCancellationToken::new();
        ct.cancel();
        let err = super::build_headers_tar(ws.path(), &["pkg/a.c".to_string()], &ct)
            .expect_err("a cancelled build stops");
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    }
}
