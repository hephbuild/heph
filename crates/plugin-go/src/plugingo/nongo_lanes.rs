//! How `_golist` sees a package's non-Go files: which ones `go list` opens, and
//! the fs-glob lanes that stage them.
//!
//! `go list` needs the *names* of most non-Go files (to resolve `//go:embed`), and
//! the *leading comment block* of the C/asm-family files in the package's own
//! directory (their build constraints). It never needs anything else, so the
//! non-Go tree reaches `_golist` through three lanes:
//!
//! - **names**: every non-Go file under the package, minus the header-read set,
//!   as a names-only glob ([`pluginfs::names_glob_addr`]) — hashed by path,
//!   staged as empty placeholders. Editing an asset moves no key.
//! - **header-read sources**: the header-read set as an ordinary content glob,
//!   staged `hashed: false, runtime: true` so `go list` reads the real files.
//! - **headers**: the provider-owned header target over that *same* glob addr
//!   ([`headers_addr`]), `hashed: true, runtime: false`: what the header-read
//!   files contribute to the key is exactly the bytes `go/build` reads. It also
//!   covers the package's `.go` files ([`go_src_glob_addr`]), by their go/build
//!   view (`crate::plugingo::go_view`), which `_golist` then stages unhashed too.
//!
//! Every pattern below is rendered from [`GO_HEADER_READ_EXTS`] and
//! [`GO_NEVER_OPENED_PREFIXES`], so the lanes partition the files by
//! construction: a file is in exactly one of them (the tests compile the
//! patterns and check it).

use hbuiltins::pluginfs;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

/// Extensions whose files `go/build` opens to read their leading comment block,
/// **case-sensitive**, without the dot.
///
/// This is `fileListForExt` (`$GOROOT/src/go/build/build.go`, and its copy in
/// `cmd/go/internal/modindex/build.go`) minus `.syso`, which `matchFile` lists
/// but never opens ("binary, no reading"). `matchFile` reads such a file with
/// `readComments`, and only when it is directly in the package directory: a
/// file in a subdirectory belongs to another package, or to none.
///
/// Verified against [`GO_HEADER_RULES_VERIFIED_AGAINST`]. The two ways a Go
/// release can move this differ in cost. One that adds an extension misreads
/// consistently: such a file stays in the names lane, staged empty, so `go list`
/// sees no header in it on every run — a wrong listing, but never a stale hit.
/// One that reads a header-read file further than `readComments` under-hashes:
/// the file is staged whole, its key covers only the extracted header, and an
/// edit past the header would be served a stale `_golist`.
pub const GO_HEADER_READ_EXTS: &[&str] = &[
    "c", "cc", "cxx", "cpp", "m", "h", "hh", "hpp", "hxx", "f", "F", "for", "f90", "s", "S", "sx",
    "swig", "swigcxx",
];

/// Name prefixes `go/build.matchFile` rejects before opening anything: a file
/// named `_x.c` or `.x.s` is never read, so it is a names-lane file.
pub const GO_NEVER_OPENED_PREFIXES: &[char] = &['_', '.'];

/// The Go release whose `go/build` sources [`GO_HEADER_READ_EXTS`],
/// [`GO_NEVER_OPENED_PREFIXES`] and the header extractor
/// ([`crate::plugingo::driver_headers`]) were checked against. A test fails when
/// the pinned default toolchain moves without this moving with it: re-read
/// `matchFile`, `fileListForExt` and `readComments` in the new release first.
///
/// It is also the scope of the `.go` trust rule
/// ([`crate::plugingo::go_view`]): `_golist` keys a `.go` file on its go/build
/// view for **every** Go version a workspace selects, not only this one — no
/// version gate (a user decision). On each bump, re-read `readGoInfo` in
/// `go/build/read.go` and the reads `cmd/go/internal/modindex` caches before
/// moving this constant.
pub const GO_HEADER_RULES_VERIFIED_AGAINST: &str = "1.27.3";

/// Which `_golist` lane a non-Go file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonGoLane {
    /// `go list` opens it and reads its leading comment block.
    HeaderRead,
    /// `go list` sees only its name.
    Names,
}

/// Classify a non-Go file by its path relative to the package directory,
/// exactly as `go/build.matchFile` decides whether to open it. `rel` is
/// `/`-separated; a `.go`, `go.mod` or `go.sum` file is not a non-Go file and
/// is not the caller's to ask about.
pub fn classify_non_go(rel: &str) -> NonGoLane {
    // Only the package's own directory is read by this package's `go list`.
    if rel.contains('/') {
        return NonGoLane::Names;
    }
    if rel.starts_with(GO_NEVER_OPENED_PREFIXES) {
        return NonGoLane::Names;
    }
    // `matchFile`: the extension starts at the *last* dot; no dot, no extension.
    let ext = rel.rfind('.').and_then(|i| rel.get(i + 1..)).unwrap_or("");
    if GO_HEADER_READ_EXTS.contains(&ext) {
        NonGoLane::HeaderRead
    } else {
        NonGoLane::Names
    }
}

/// `"<pkg>/"`, or `""` for the root package.
fn dir_prefix(pkg: &str) -> String {
    if pkg.is_empty() {
        String::new()
    } else {
        format!("{pkg}/")
    }
}

/// The header-read files of `pkg`'s own directory, as a content glob: one wax
/// alternation over [`GO_HEADER_READ_EXTS`], minus the never-opened prefixes.
///
/// The **one** builder of this addr: `_golist`'s unhashed staging lane and the
/// header target's input are both this exact string, so one request walks it
/// once and the bytes `go list` reads are the bytes the header target hashed.
pub fn header_read_glob_addr(pkg: &str) -> Addr {
    let dir = dir_prefix(pkg);
    let pattern = format!("{dir}*.{{{}}}", GO_HEADER_READ_EXTS.join(","));
    let excludes: Vec<String> = GO_NEVER_OPENED_PREFIXES
        .iter()
        .map(|p| format!("{dir}{p}*"))
        .collect();
    let excludes: Vec<&str> = excludes.iter().map(String::as_str).collect();
    pluginfs::glob_addr(&pattern, &excludes)
}

/// The `.go` files of `pkg`'s own directory, as a content glob (`_test.go`
/// included: `go list -test` reads them the same way).
///
/// The **one** builder of this addr, for the same reason as
/// [`header_read_glob_addr`]: `_golist` stages it unhashed (a non-vendored
/// module) and the header target hashes the go/build view of the very files it
/// lists.
///
/// `_x.go` and `.x.go` stay in, although `matchFile` never opens them
/// ([`GO_NEVER_OPENED_PREFIXES`]): their view is hashed for nothing, so editing
/// one re-lists — an over-hash, never a stale hit, and not worth a second glob
/// shape for a file Go itself ignores.
pub fn go_src_glob_addr(pkg: &str) -> Addr {
    pluginfs::glob_addr(&format!("{}*.go", dir_prefix(pkg)), &[])
}

/// Excludes every lane-less file shares: the `.go` sources (their own lane) and
/// the module files (the `_go_mod` lane delivers them; staging them twice is an
/// output collision).
const NOT_NON_GO: &[&str] = &["**/*.go", "**/go.mod", "**/go.sum"];

/// The names lane of `pkg`: every non-Go file of its subtree (the whole module
/// for the root package) minus the header-read set, names-only.
///
/// The header-read set is excluded with one glob per extension rather than one
/// alternation: an fs glob's `e` arg is comma-separated, so a `{…,…}` pattern
/// cannot be an exclude. `[!_.]` is the never-opened-prefix rule, rendered from
/// [`GO_NEVER_OPENED_PREFIXES`].
pub fn names_glob_addr(pkg: &str) -> Addr {
    let dir = dir_prefix(pkg);
    let not_prefix: String = GO_NEVER_OPENED_PREFIXES.iter().collect();
    let header_read: Vec<String> = GO_HEADER_READ_EXTS
        .iter()
        .map(|ext| format!("{dir}[!{not_prefix}]*.{ext}"))
        .collect();
    let excludes: Vec<&str> = NOT_NON_GO
        .iter()
        .copied()
        .chain(header_read.iter().map(String::as_str))
        .collect();
    pluginfs::names_glob_addr(&format!("{dir}**/*"), &excludes)
}

/// Today's content lane, kept for a vendored module (see
/// `vendored_module`): every non-Go file under the package, content-hashed.
pub fn content_glob_addr(pkg: &str) -> Addr {
    pluginfs::glob_addr(&format!("{}**/*", dir_prefix(pkg)), NOT_NON_GO)
}

/// Whether the module rooted at `module_root` vendors its dependencies. Such a
/// module keeps the content lane: Go turns on `-mod=vendor` by itself when the
/// module has a `vendor/` directory, and `go list` then reads
/// `vendor/modules.txt`, so a names-only placeholder for it would break the
/// listing. Keying on `modules.txt` rather than the directory diverges from Go
/// in no case that lists: a `vendor/` without it fails `go list` ("inconsistent
/// vendoring") in either lane. A stat on every call — never memoized, so adding
/// or removing `vendor/` is seen on the next run.
pub fn vendored_module(module_root: &std::path::Path) -> bool {
    module_root.join("vendor").join("modules.txt").is_file()
}

/// Provider package of the header targets. `//@heph/go/headers/<pkg>:headers`
/// for package `<pkg>`, `//@heph/go/headers:headers` for the root package.
///
/// Provider-owned on purpose: the buildfile provider never reads a BUILD file
/// under `@heph/…` (its `is_reserved_package`, pinned by
/// `packages_under_heph_are_never_read`), so a user target can never stand in
/// for `_golist`'s only hashed view of the header-read files.
pub const HEADERS_PKG_PREFIX: &str = "@heph/go/headers";
/// Target name of a header target.
pub const HEADERS_NAME: &str = "headers";
/// The header target's single output file.
pub const HEADERS_OUT: &str = "headers.tar";

/// Header-target arg: the package of the `go_codegen_root` state that scopes the
/// consumer's `go_src` query. Absent when none is declared.
pub const GEN_ROOT_ARG: &str = "gen_root";
/// Header-target arg: the consumer's `go_codegen_deps`, absolute, `\n`-joined
/// and hex-encoded — an addr arg has no escapes, and a dep is an addr that may
/// itself carry `"`, `[…]` or `,`. Absent when there are none.
pub const GEN_DEPS_ARG: &str = "gen_deps";

/// The header target of `pkg`. Variant-free: `go/build` reads the same prefix
/// whatever the GOOS/GOARCH/tags, and `_golist` decides what it means.
///
/// `gen_root` and `gen_deps` are what the consumer read of its codegen states
/// (see `provider::compute_pkg_src_addrs`). They ride in the args because the
/// header target's own `get` sees only the states of `@heph/…`'s ancestry, not
/// the package's; a package with neither keeps the bare addr.
pub fn headers_addr(pkg: &str, gen_root: Option<&str>, gen_deps: &[String]) -> Addr {
    let hpkg = if pkg.is_empty() {
        HEADERS_PKG_PREFIX.to_string()
    } else {
        format!("{HEADERS_PKG_PREFIX}/{pkg}")
    };
    let mut args = std::collections::BTreeMap::new();
    if let Some(root) = gen_root {
        args.insert(GEN_ROOT_ARG.to_string(), root.to_string());
    }
    if !gen_deps.is_empty() {
        args.insert(GEN_DEPS_ARG.to_string(), hex::encode(gen_deps.join("\n")));
    }
    Addr::new(PkgBuf::from(hpkg), HEADERS_NAME.to_string(), args)
}

/// The `(gen_root, gen_deps)` a header addr carries — the inverse of
/// [`headers_addr`]. An arg it does not know fails: a header target answered
/// with some of its inputs ignored would key `_golist` on less than it stages.
pub fn headers_codegen_args(addr: &Addr) -> anyhow::Result<(Option<String>, Vec<String>)> {
    use anyhow::Context as _;
    // Named by its package, not by the addr: the addr's `gen_deps` is hex.
    let owner = || match pkg_from_headers_pkg(addr.package.as_str()) {
        Some(pkg) => format!("the header target of //{pkg}"),
        None => format!("header target {}", addr.package.as_str()),
    };
    if let Some(k) = addr
        .args
        .keys()
        .find(|k| k.as_str() != GEN_ROOT_ARG && k.as_str() != GEN_DEPS_ARG)
    {
        anyhow::bail!("{}: unknown arg `{k}`", owner());
    }
    let deps = match addr.args.get(GEN_DEPS_ARG) {
        Some(h) => {
            let raw = hex::decode(h)
                .with_context(|| format!("{}: `{GEN_DEPS_ARG}` is not hex: {h}", owner()))?;
            String::from_utf8(raw)
                .with_context(|| format!("{}: `{GEN_DEPS_ARG}` is not UTF-8", owner()))?
                .split('\n')
                .map(str::to_string)
                .collect()
        }
        None => Vec::new(),
    };
    Ok((addr.args.get(GEN_ROOT_ARG).cloned(), deps))
}

/// A header addr in plain words, its codegen args decoded — what an error names
/// instead of the hex `gen_deps`. Falls back to the addr itself when it is not
/// one, or its args do not decode.
pub fn describe_headers_addr(addr: &Addr) -> String {
    let (Some(pkg), Ok((root, deps))) = (
        pkg_from_headers_pkg(addr.package.as_str()),
        headers_codegen_args(addr),
    ) else {
        return addr.format();
    };
    let mut s = format!("the header target of //{pkg}");
    if let Some(root) = root {
        s.push_str(&format!(", go_codegen_root //{root}"));
    }
    if !deps.is_empty() {
        s.push_str(&format!(", go_codegen_deps [{}]", deps.join(", ")));
    }
    s
}

/// The Go package a header-target package stands for, or `None` if `hpkg` is not
/// one.
pub fn pkg_from_headers_pkg(hpkg: &str) -> Option<&str> {
    let rest = hpkg.strip_prefix(HEADERS_PKG_PREFIX)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/').filter(|p| !p.is_empty())
}

/// Spec of the header target of `pkg`. Its inputs are
/// [`header_read_glob_addr`], [`go_src_glob_addr`] and the codegen addrs
/// `go_gen` (`provider::codegen_addrs`, from the addr's args); it is derived from
/// the addr alone, so answering it reads nothing from disk.
pub fn headers_spec(addr: Addr, pkg: &str, go_gen: &[String]) -> TargetSpec {
    let mut config = HashMap::from([
        (
            "src".to_string(),
            Value::String(header_read_glob_addr(pkg).format()),
        ),
        (
            "go_src".to_string(),
            Value::String(go_src_glob_addr(pkg).format()),
        ),
    ]);
    if !go_gen.is_empty() {
        config.insert(
            "go_gen".to_string(),
            Value::List(go_gen.iter().cloned().map(Value::String).collect()),
        );
    }
    TargetSpec {
        addr,
        driver: crate::plugingo::listed::DRIVER_HEADERS.to_string(),
        config,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every T12 adversarial name, with the lane `go/build` puts it in. Paths
    /// are relative to the package directory.
    const T12: &[(&str, NonGoLane)] = &[
        ("a.s", NonGoLane::HeaderRead),
        ("a.S", NonGoLane::HeaderRead),
        ("a.C", NonGoLane::Names),
        ("a.H", NonGoLane::Names),
        ("a.SYSO", NonGoLane::Names),
        ("a.syso", NonGoLane::Names),
        ("a.c.bak", NonGoLane::Names),
        ("a.go.txt", NonGoLane::Names),
        ("a.h~", NonGoLane::Names),
        ("c", NonGoLane::Names),
        (".c", NonGoLane::Names),
        ("_a.c", NonGoLane::Names),
        (".hidden.s", NonGoLane::Names),
        ("a.f90", NonGoLane::HeaderRead),
        ("a.f9", NonGoLane::Names),
        // NFC and NFD spellings of `ü.c`: both header-read, raw bytes as named.
        ("\u{fc}.c", NonGoLane::HeaderRead),
        ("u\u{308}.c", NonGoLane::HeaderRead),
        // GOOS/GOARCH suffixes: header-read whatever the variant (over-hashing
        // only — a `_windows.c` header cannot change a linux listing's answer,
        // but tracking it keeps the lanes variant-free).
        ("a_linux.s", NonGoLane::HeaderRead),
        ("a_windows.c", NonGoLane::HeaderRead),
        ("sub/a.c", NonGoLane::Names),
        ("sub/deeper/a.s", NonGoLane::Names),
        ("README.md", NonGoLane::Names),
        ("BUILD", NonGoLane::Names),
        ("testdata/x.c", NonGoLane::Names),
        ("a.cc", NonGoLane::HeaderRead),
        ("a.cxx", NonGoLane::HeaderRead),
        ("a.cpp", NonGoLane::HeaderRead),
        ("a.m", NonGoLane::HeaderRead),
        ("a.h", NonGoLane::HeaderRead),
        ("a.hh", NonGoLane::HeaderRead),
        ("a.hpp", NonGoLane::HeaderRead),
        ("a.hxx", NonGoLane::HeaderRead),
        ("a.f", NonGoLane::HeaderRead),
        ("a.F", NonGoLane::HeaderRead),
        ("a.for", NonGoLane::HeaderRead),
        ("a.sx", NonGoLane::HeaderRead),
        ("a.swig", NonGoLane::HeaderRead),
        ("a.swigcxx", NonGoLane::HeaderRead),
    ];

    /// T12: the classifier agrees with `go/build.matchFile` on every
    /// adversarial name.
    #[test]
    fn classifier_matches_go_build() {
        for (name, want) in T12 {
            assert_eq!(classify_non_go(name), *want, "{name:?}");
        }
    }

    /// One compiled fs glob, matched the way the fs driver matches it: the
    /// include pattern, minus any exclude, against the ws-relative path.
    struct Lane {
        include: wax::Glob<'static>,
        excludes: Vec<wax::Glob<'static>>,
    }

    impl Lane {
        fn of(addr: &Addr) -> Self {
            let p = addr.args.get("p").expect("glob addr has p");
            let excludes = addr
                .args
                .get("e")
                .map(|e| {
                    e.split(',')
                        .map(|s| wax::Glob::new(s).expect("exclude compiles").into_owned())
                        .collect()
                })
                .unwrap_or_default();
            Self {
                include: wax::Glob::new(p).expect("pattern compiles").into_owned(),
                excludes,
            }
        }

        fn matches(&self, path: &str) -> bool {
            use wax::Program as _;
            self.include.is_match(path) && !self.excludes.iter().any(|e| e.is_match(path))
        }
    }

    /// T11b: every non-Go name lands in exactly one lane, and the one
    /// [`classify_non_go`] names — for a nested package and the root package alike. The
    /// names lane is therefore disjoint from the header-read lane, which is what
    /// keeps a placeholder from ever being staged over real content.
    #[test]
    fn lanes_partition_the_non_go_files() {
        for pkg in ["", "pkg", "a/b"] {
            let names = Lane::of(&names_glob_addr(pkg));
            let header = Lane::of(&header_read_glob_addr(pkg));
            for (name, want) in T12 {
                let path = format!("{}{name}", dir_prefix(pkg));
                let in_names = names.matches(&path);
                let in_header = header.matches(&path);
                assert!(
                    in_names != in_header,
                    "{path:?} must be in exactly one lane (names={in_names}, header={in_header})"
                );
                let got = if in_header {
                    NonGoLane::HeaderRead
                } else {
                    NonGoLane::Names
                };
                assert_eq!(got, *want, "{path:?}");
            }
            let go = Lane::of(&go_src_glob_addr(pkg));
            for (name, _) in T12 {
                let path = format!("{}{name}", dir_prefix(pkg));
                assert!(!go.matches(&path), "{path:?} is not a .go file");
            }
            // Never a non-Go file: on no non-Go lane at all; a `.go` file of the
            // package directory is on the go lane alone.
            for (not_non_go, on_go_lane) in [
                ("a.go", true),
                ("a_test.go", true),
                ("a_linux_test.go", true),
                ("go.mod", false),
                ("go.sum", false),
                ("sub/go.mod", false),
                ("sub/b.go", false),
            ] {
                let path = format!("{}{not_non_go}", dir_prefix(pkg));
                assert!(!names.matches(&path), "{path:?} is not a non-Go file");
                assert!(!header.matches(&path), "{path:?} is not a non-Go file");
                assert_eq!(go.matches(&path), on_go_lane, "{path:?} on the go lane");
            }
        }
    }

    /// Every regular file under `dir`, as a `/`-separated path relative to it,
    /// spelled as the filesystem lists it (the fs driver matches listed names).
    fn listed_files(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("read_dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path.strip_prefix(dir).expect("under dir");
                let rel: Vec<&str> = rel
                    .components()
                    .map(|c| c.as_os_str().to_str().expect("utf-8 name"))
                    .collect();
                out.push(rel.join("/"));
            }
        }
        out.sort();
        out
    }

    /// T11b on a real directory: files created on disk — so a case-insensitive
    /// or normalizing filesystem (APFS) spells the names it lists — each land in
    /// exactly the lane `go/build` puts them in. Stems are distinct, since `a.S`
    /// and `a.s` cannot coexist on a case-insensitive volume.
    #[test]
    fn lanes_partition_files_listed_from_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pkg_dir = tmp.path().join("pkg");
        let cases: &[(&str, NonGoLane)] = &[
            ("a.S", NonGoLane::HeaderRead),
            ("b.C", NonGoLane::Names),
            ("\u{fc}.c", NonGoLane::HeaderRead),
            ("d.c", NonGoLane::HeaderRead),
            ("e.s", NonGoLane::HeaderRead),
            ("_f.c", NonGoLane::Names),
            (".g.s", NonGoLane::Names),
            ("sub/h.c", NonGoLane::Names),
            ("README.md", NonGoLane::Names),
        ];
        for (name, _) in cases {
            let p = pkg_dir.join(name);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, "// header\n").expect("write");
        }
        let listed = listed_files(tmp.path());
        assert_eq!(
            listed.len(),
            cases.len(),
            "every file is listed: {listed:?}"
        );

        let names = Lane::of(&names_glob_addr("pkg"));
        let header = Lane::of(&header_read_glob_addr("pkg"));
        for path in &listed {
            let in_names = names.matches(path);
            let in_header = header.matches(path);
            assert!(
                in_names != in_header,
                "{path:?} must be in exactly one lane (names={in_names}, header={in_header})"
            );
            let got = if in_header {
                NonGoLane::HeaderRead
            } else {
                NonGoLane::Names
            };
            let rel = path.strip_prefix("pkg/").expect("under pkg");
            assert_eq!(got, classify_non_go(rel), "{path:?}: glob vs classifier");
            // APFS and ext4 both preserve the spelling a name was created with.
            let want = cases
                .iter()
                .find(|(name, _)| *name == rel)
                .map(|(_, lane)| *lane);
            assert_eq!(Some(got), want, "{path:?}");
        }
    }

    /// A nested package's header-read lane is its own directory only, and its
    /// names lane its own subtree only: a sibling package's files are on neither.
    #[test]
    fn lanes_stay_inside_the_package() {
        let names = Lane::of(&names_glob_addr("pkg"));
        let header = Lane::of(&header_read_glob_addr("pkg"));
        for outside in ["other/a.c", "pkgx/a.c", "a.c", "README.md"] {
            assert!(!names.matches(outside), "{outside:?}");
            assert!(!header.matches(outside), "{outside:?}");
        }
    }

    /// T20: the D4 rules record the Go release they were verified against, and
    /// that has to be the pinned default toolchain. If this fails, the default
    /// moved: re-read `matchFile`, `fileListForExt` and `readComments` in the
    /// new release (`$GOROOT/src/go/build/{build,read}.go`), update the rules and
    /// the extractor if they changed, then move the recorded version.
    #[test]
    fn header_rules_are_verified_against_the_pinned_go() {
        assert_eq!(
            GO_HEADER_RULES_VERIFIED_AGAINST,
            crate::plugingo::toolchain::DEFAULT_GO_VERSION,
            "the pinned Go moved: re-verify GO_HEADER_READ_EXTS and the header extractor"
        );
    }

    #[test]
    fn headers_addr_round_trips() {
        for pkg in ["", "pkg", "a/b/c"] {
            let addr = headers_addr(pkg, None, &[]);
            assert_eq!(addr.name, HEADERS_NAME);
            assert_eq!(pkg_from_headers_pkg(addr.package.as_str()), Some(pkg));
            assert!(addr.args.is_empty(), "variant-free: {addr}");
        }
        assert_eq!(
            headers_addr("pkg", None, &[]).format(),
            "//@heph/go/headers/pkg:headers"
        );
        assert_eq!(
            headers_addr("", None, &[]).format(),
            "//@heph/go/headers:headers"
        );
        assert_eq!(pkg_from_headers_pkg("@heph/go/headersx"), None);
        assert_eq!(pkg_from_headers_pkg("@heph/go/headers/"), None);
        assert_eq!(pkg_from_headers_pkg("pkg"), None);
    }

    /// The codegen config a header addr carries survives a format/parse round
    /// trip, including deps that are addrs with quotes, filters and commas — and
    /// an arg the header target does not know is refused.
    #[test]
    fn headers_addr_carries_the_codegen_config() {
        let deps = vec![
            "//gen:a".to_string(),
            "//gen:b@x=\"1,2\"|out[a.go,{b,c}.go]".to_string(),
        ];
        for root in [None, Some(""), Some("a/b")] {
            let addr = headers_addr("pkg", root, &deps);
            let parsed = hmodel::htaddr::parse_addr(&addr.format()).expect("parse");
            assert_eq!(parsed, addr, "round trip: {addr}");
            let (got_root, got_deps) = headers_codegen_args(&parsed).expect("args");
            assert_eq!(got_root.as_deref(), root);
            assert_eq!(got_deps, deps);
        }
        let mut bad = headers_addr("pkg", None, &[]).args.clone();
        bad.insert("v".to_string(), "host".to_string());
        let bad = Addr::new(
            PkgBuf::from("@heph/go/headers/pkg"),
            HEADERS_NAME.to_string(),
            bad,
        );
        assert!(headers_codegen_args(&bad).is_err());
    }

    /// B2: the header target's input is the exact glob addr `_golist` stages
    /// unhashed — one string, so one walk and one memoized artifact.
    #[test]
    fn header_target_reads_the_glob_golist_stages() {
        let spec = headers_spec(headers_addr("pkg", None, &[]), "pkg", &[]);
        assert_eq!(
            spec.config.get("src"),
            Some(&Value::String(header_read_glob_addr("pkg").format()))
        );
        assert_eq!(
            spec.config.get("go_src"),
            Some(&Value::String(go_src_glob_addr("pkg").format()))
        );
        assert_eq!(spec.driver, crate::plugingo::listed::DRIVER_HEADERS);
    }

    /// Pins the rendered patterns, so a change to them is a deliberate one.
    #[test]
    fn lane_patterns_are_pinned() {
        let h = header_read_glob_addr("pkg");
        assert_eq!(
            h.args.get("p").map(String::as_str),
            Some("pkg/*.{c,cc,cxx,cpp,m,h,hh,hpp,hxx,f,F,for,f90,s,S,sx,swig,swigcxx}")
        );
        assert_eq!(h.args.get("e").map(String::as_str), Some("pkg/_*,pkg/.*"));
        let n = names_glob_addr("");
        assert_eq!(n.args.get("p").map(String::as_str), Some("**/*"));
        let e = n.args.get("e").expect("names excludes");
        assert!(
            e.starts_with("**/*.go,**/go.mod,**/go.sum,[!_.]*.c,"),
            "{e}"
        );
    }
}
