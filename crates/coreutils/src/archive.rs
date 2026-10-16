//! `tar`, `gzip`/`gunzip` and `zstd` — reproducible by default.
//!
//! These are the applets where the divergence is not a flag but the *bytes*.
//! GNU tar and bsdtar disagree about which of `--transform`, `--sort`,
//! `--owner` and `--mtime` exist at all; `gzip` writes the source filename and
//! its mtime into the header unless told not to; and `zstd` is not installed by
//! default on either host. Archiving the same tree twice, on two machines,
//! should produce the same bytes — and with the host tools it does not.
//!
//! So the reproducible settings are the *defaults* here, not flags anyone has
//! to remember:
//!
//! * entries sorted by path, so the archive does not inherit directory order;
//! * uid/gid 0 and empty owner names, so it does not inherit whoever built it;
//! * mtime from `SOURCE_DATE_EPOCH` when set, otherwise 0;
//! * no gzip header name or timestamp.
//!
//! There is deliberately no flag to turn any of that off. A recipe that wants a
//! non-reproducible archive is a recipe with a bug.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const OK: i32 = 0;
const ERROR: i32 = 1;

/// Timestamp stamped into every entry.
///
/// `SOURCE_DATE_EPOCH` is the cross-ecosystem convention for exactly this, so a
/// workspace that already sets it gets archives that agree with everything else
/// it builds. Without it, 0 — a real timestamp is the single most common reason
/// two builds of the same tree differ.
fn source_date_epoch() -> u64 {
    parse_epoch(std::env::var("SOURCE_DATE_EPOCH").ok().as_deref())
}

/// The parsing half, split out so it can be tested without touching the
/// process environment.
///
/// Mutating `SOURCE_DATE_EPOCH` from a test is not a local act: the test
/// harness is one process, tests run in parallel, and anything else reading it
/// — such as the archive writer — sees the change. That is exactly how the
/// original version of this test passed locally and failed on CI.
fn parse_epoch(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

fn fail(program: &str, msg: impl std::fmt::Display) -> i32 {
    eprintln!("{program}: {msg}");
    ERROR
}

// ---------------------------------------------------------------- tar

#[derive(Debug, Default)]
struct TarOptions {
    create: bool,
    extract: bool,
    list: bool,
    gzip: bool,
    zstd: bool,
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

fn parse_tar(argv: &[OsString]) -> Result<TarOptions, String> {
    let mut o = TarOptions::default();
    let mut it = argv.iter().skip(1).peekable();
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        if !arg.starts_with('-') {
            o.paths.push(PathBuf::from(raw));
            continue;
        }
        if arg == "--" {
            o.paths.extend(it.map(PathBuf::from));
            break;
        }
        let flags = arg.trim_start_matches('-');
        let mut chars = flags.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                'c' => o.create = true,
                'x' => o.extract = true,
                't' => o.list = true,
                'z' => o.gzip = true,
                'v' => {}
                'f' => {
                    let rest: String = chars.by_ref().collect();
                    let value = if rest.is_empty() {
                        it.next().map(PathBuf::from)
                    } else {
                        Some(PathBuf::from(rest))
                    };
                    match value {
                        Some(v) => o.file = Some(v),
                        None => return Err("option requires an argument -- f".to_string()),
                    }
                }
                'C' => {
                    let rest: String = chars.by_ref().collect();
                    let value = if rest.is_empty() {
                        it.next().map(PathBuf::from)
                    } else {
                        Some(PathBuf::from(rest))
                    };
                    match value {
                        Some(v) => o.dir = Some(v),
                        None => return Err("option requires an argument -- C".to_string()),
                    }
                }
                other => return Err(format!("invalid option -- {other}")),
            }
        }
    }
    match (o.create, o.extract, o.list) {
        (true, false, false) | (false, true, false) | (false, false, true) => {}
        (false, false, false) => {
            return Err("one of -c, -x or -t is required".to_string());
        }
        _ => return Err("-c, -x and -t are mutually exclusive".to_string()),
    }
    Ok(o)
}

/// Every file and symlink under `root`, relative to it, sorted.
///
/// Sorted because directory order is filesystem- and machine-dependent, and it
/// would otherwise end up baked into the archive's bytes.
fn walk_sorted(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_dir() {
            continue;
        }
        out.push(entry.into_path());
    }
    out.sort();
    Ok(out)
}

/// Add `path` under `name`, with every machine-specific field flattened.
fn append_reproducible<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &Path,
    name: &Path,
    mtime: u64,
) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let mut header = tar::Header::new_gnu();
    header.set_mtime(mtime);
    header.set_uid(0);
    header.set_gid(0);
    // Names, not just ids: a numeric 0 with "raphael" beside it still records
    // who built it.
    header.set_username("")?;
    header.set_groupname("")?;

    if meta.is_symlink() {
        let target = std::fs::read_link(path)?;
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        // Mode is normalised too — a symlink's own bits are not portable.
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_link(&mut header, name, &target)
    } else {
        // Only the executable bit survives; the rest is umask, which is
        // ambient state and has no business in an artifact.
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            if meta.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            }
        };
        header.set_mode(mode);
        header.set_size(meta.len());
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        let mut f = File::open(path)?;
        builder.append_data(&mut header, name, &mut f)
    }
}

fn tar_create(o: &TarOptions) -> io::Result<()> {
    let Some(file) = &o.file else {
        return Err(io::Error::other("-f is required"));
    };
    let base = o.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let out = File::create(file)?;
    let out = BufWriter::new(out);
    let mtime = source_date_epoch();

    // The compressor is chosen here rather than by extension: guessing from a
    // name is how you end up with a `.tar.gz` that is not gzipped.
    let mut sink: Box<dyn Write> = if o.gzip {
        Box::new(
            flate2::GzBuilder::new()
                .mtime(0)
                .write(out, flate2::Compression::default()),
        )
    } else if o.zstd {
        Box::new(zstd::stream::Encoder::new(out, 3)?.auto_finish())
    } else {
        Box::new(out)
    };
    {
        let mut builder = tar::Builder::new(&mut sink);
        builder.follow_symlinks(false);
        let mut members: Vec<(PathBuf, PathBuf)> = Vec::new();
        for p in &o.paths {
            let full = base.join(p);
            if full.is_dir() {
                for found in walk_sorted(&full)? {
                    let rel = found.strip_prefix(&base).unwrap_or(&found).to_path_buf();
                    members.push((found, rel));
                }
            } else {
                members.push((full, p.clone()));
            }
        }
        members.sort_by(|a, b| a.1.cmp(&b.1));
        for (full, rel) in &members {
            append_reproducible(&mut builder, full, rel, mtime)?;
        }
        builder.finish()?;
    }
    sink.flush()
}

fn open_maybe_compressed(path: &Path) -> io::Result<Box<dyn Read>> {
    let f = BufReader::new(File::open(path)?);
    let mut probe = f;
    let mut magic = [0u8; 4];
    let read = probe.read(&mut magic)?;
    let head = magic.get(..read).unwrap_or(&[]);
    let rest: Box<dyn Read> = Box::new(io::Cursor::new(head.to_vec()).chain(probe));
    // Detected from the bytes, not the name: an archive named `.tar` that is
    // gzipped should still extract rather than fail confusingly.
    if head.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(rest)))
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Ok(Box::new(zstd::stream::Decoder::new(rest)?))
    } else {
        Ok(rest)
    }
}

fn tar_extract(o: &TarOptions) -> io::Result<()> {
    let Some(file) = &o.file else {
        return Err(io::Error::other("-f is required"));
    };
    let dest = o.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dest)?;
    let mut archive = tar::Archive::new(open_maybe_compressed(file)?);
    archive.set_overwrite(true);
    archive.unpack(&dest)
}

fn tar_list(o: &TarOptions) -> io::Result<()> {
    let Some(file) = &o.file else {
        return Err(io::Error::other("-f is required"));
    };
    let mut archive = tar::Archive::new(open_maybe_compressed(file)?);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for entry in archive.entries()? {
        let entry = entry?;
        writeln!(out, "{}", entry.path()?.display())?;
    }
    Ok(())
}

pub fn tar(argv: Vec<OsString>) -> i32 {
    let program = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("tar")
        .to_string();
    let o = match parse_tar(&argv) {
        Ok(o) => o,
        Err(e) => return fail(&program, e),
    };
    let res = if o.create {
        tar_create(&o)
    } else if o.extract {
        tar_extract(&o)
    } else {
        tar_list(&o)
    };
    match res {
        Ok(()) => OK,
        Err(e) => fail(&program, e),
    }
}

// ------------------------------------------------------- gzip / zstd

#[derive(Debug, Default)]
struct CompressOptions {
    decompress: bool,
    to_stdout: bool,
    keep: bool,
    level: Option<u32>,
    files: Vec<PathBuf>,
}

fn parse_compress(
    argv: &[OsString],
    decompress_by_default: bool,
) -> Result<CompressOptions, String> {
    let mut o = CompressOptions {
        decompress: decompress_by_default,
        ..CompressOptions::default()
    };
    let mut it = argv.iter().skip(1);
    for raw in it.by_ref() {
        let arg = raw.to_string_lossy().into_owned();
        if arg == "-" || !arg.starts_with('-') {
            o.files.push(PathBuf::from(raw));
            continue;
        }
        if arg == "--" {
            break;
        }
        for c in arg.chars().skip(1) {
            match c {
                'd' => o.decompress = true,
                'c' => o.to_stdout = true,
                'k' => o.keep = true,
                'f' | 'q' | 'n' => {}
                '1'..='9' => o.level = c.to_digit(10),
                other => return Err(format!("invalid option -- {other}")),
            }
        }
    }
    o.files.extend(it.map(PathBuf::from));
    Ok(o)
}

/// Wrap `w` in the compressor for `kind`.
fn encoder(kind: Kind, w: Box<dyn Write>, level: Option<u32>) -> io::Result<Box<dyn Write>> {
    Ok(match kind {
        // `GzBuilder` with no filename and no mtime: gzip normally records both,
        // which is why gzipping the same bytes twice gives two different files.
        Kind::Gzip => Box::new(
            flate2::GzBuilder::new()
                .mtime(0)
                .write(w, flate2::Compression::new(level.unwrap_or(6).min(9))),
        ),
        Kind::Zstd => {
            Box::new(zstd::stream::Encoder::new(w, level.unwrap_or(3) as i32)?.auto_finish())
        }
    })
}

fn decoder(kind: Kind, r: Box<dyn Read>) -> io::Result<Box<dyn Read>> {
    Ok(match kind {
        Kind::Gzip => Box::new(flate2::read::GzDecoder::new(r)),
        Kind::Zstd => Box::new(zstd::stream::Decoder::new(r)?),
    })
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Gzip,
    Zstd,
}

impl Kind {
    fn suffix(self) -> &'static str {
        match self {
            Kind::Gzip => "gz",
            Kind::Zstd => "zst",
        }
    }
}

fn compress_main(kind: Kind, argv: Vec<OsString>, decompress_by_default: bool) -> i32 {
    let program = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("gzip")
        .to_string();
    let o = match parse_compress(&argv, decompress_by_default) {
        Ok(o) => o,
        Err(e) => return fail(&program, e),
    };

    // No operands is the stream form: `cat x | gzip > x.gz`.
    if o.files.is_empty() {
        let res = (|| -> io::Result<()> {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let r: Box<dyn Read> = Box::new(stdin.lock());
            let w: Box<dyn Write> = Box::new(stdout.lock());
            if o.decompress {
                let mut d = decoder(kind, r)?;
                let mut w = w;
                io::copy(&mut d, &mut w)?;
                w.flush()
            } else {
                let mut e = encoder(kind, w, o.level)?;
                let mut r = r;
                io::copy(&mut r, &mut e)?;
                e.flush()
            }
        })();
        return match res {
            Ok(()) => OK,
            Err(e) => fail(&program, e),
        };
    }

    for path in &o.files {
        let res = (|| -> io::Result<()> {
            let target = if o.decompress {
                path.to_string_lossy()
                    .strip_suffix(&format!(".{}", kind.suffix()))
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::other(format!(
                            "{}: unknown suffix — expected .{}",
                            path.display(),
                            kind.suffix()
                        ))
                    })?
            } else {
                PathBuf::from(format!("{}.{}", path.display(), kind.suffix()))
            };

            let src: Box<dyn Read> = Box::new(BufReader::new(File::open(path)?));
            let stdout = io::stdout();
            let dst: Box<dyn Write> = if o.to_stdout {
                Box::new(stdout.lock())
            } else {
                Box::new(BufWriter::new(File::create(&target)?))
            };

            if o.decompress {
                let mut d = decoder(kind, src)?;
                let mut dst = dst;
                io::copy(&mut d, &mut dst)?;
                dst.flush()?;
            } else {
                let mut e = encoder(kind, dst, o.level)?;
                let mut src = src;
                io::copy(&mut src, &mut e)?;
                e.flush()?;
            }
            // gzip replaces its input unless told to keep it; `-c` never does,
            // because it did not create the destination.
            if !o.keep && !o.to_stdout {
                std::fs::remove_file(path)?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            return fail(&program, e);
        }
    }
    OK
}

pub fn gzip(argv: Vec<OsString>) -> i32 {
    compress_main(Kind::Gzip, argv, false)
}

pub fn gunzip(argv: Vec<OsString>) -> i32 {
    compress_main(Kind::Gzip, argv, true)
}

pub fn zstd(argv: Vec<OsString>) -> i32 {
    compress_main(Kind::Zstd, argv, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn tar_requires_exactly_one_mode() {
        parse_tar(&argv(&["tar", "-f", "x"])).expect_err("no mode is an error");
        parse_tar(&argv(&["tar", "-cxf", "x"])).expect_err("two modes are an error");
        parse_tar(&argv(&["tar", "-cf", "x", "a"])).expect("one mode is fine");
    }

    #[test]
    fn tar_f_takes_an_attached_or_separate_value() {
        let a = parse_tar(&argv(&["tar", "-cfout.tar", "p"])).expect("attached");
        assert_eq!(a.file, Some(PathBuf::from("out.tar")));
        let b = parse_tar(&argv(&["tar", "-cf", "out.tar", "p"])).expect("separate");
        assert_eq!(b.file, Some(PathBuf::from("out.tar")));
    }

    /// The property the whole module exists for.
    #[test]
    fn the_same_tree_tars_to_the_same_bytes() {
        let build = |dir: &Path, out: &Path| {
            write(dir, "b.txt", "bee");
            write(dir, "a.txt", "aye");
            write(dir, "sub/c.txt", "sea");
            let code = tar(argv(&[
                "tar",
                "-cf",
                &out.to_string_lossy(),
                "-C",
                &dir.to_string_lossy(),
                ".",
            ]));
            assert_eq!(code, OK);
            std::fs::read(out).unwrap()
        };

        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let outs = tempfile::tempdir().unwrap();
        let a = build(one.path(), &outs.path().join("a.tar"));
        let b = build(two.path(), &outs.path().join("b.tar"));
        assert_eq!(a, b, "the same tree must produce byte-identical archives");
    }

    #[test]
    fn a_tar_records_neither_the_builder_nor_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", "x");
        let out = dir.path().join("a.tar");
        assert_eq!(
            tar(argv(&[
                "tar",
                "-cf",
                &out.to_string_lossy(),
                "-C",
                &dir.path().to_string_lossy(),
                "f",
            ])),
            OK
        );
        // The declared epoch, whatever it is — this must not assume it is
        // unset, because the devenv shell and CI both export
        // `SOURCE_DATE_EPOCH`, and a test that hardcodes 0 passes only where
        // nothing set it.
        let declared = source_date_epoch();
        let on_disk = std::fs::metadata(dir.path().join("f"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut archive = tar::Archive::new(File::open(&out).unwrap());
        let mut seen = 0;
        for entry in archive.entries().unwrap() {
            let e = entry.unwrap();
            let h = e.header();
            assert_eq!(h.uid().unwrap(), 0, "uid must be flattened");
            assert_eq!(h.gid().unwrap(), 0, "gid must be flattened");
            assert_eq!(
                h.mtime().unwrap(),
                declared,
                "mtime must be the declared epoch"
            );
            assert_ne!(
                h.mtime().unwrap(),
                on_disk,
                "mtime must not be the file's own — that is the clock leaking in"
            );
            seen += 1;
        }
        assert_eq!(seen, 1);
    }

    #[test]
    fn source_date_epoch_is_parsed_or_defaults_to_zero() {
        assert_eq!(parse_epoch(Some("1234567890")), 1_234_567_890);
        assert_eq!(parse_epoch(None), 0, "no epoch means the start of time");
        assert_eq!(parse_epoch(Some("")), 0, "an empty value is not a date");
        assert_eq!(parse_epoch(Some("tomorrow")), 0, "nor is prose");
    }

    #[test]
    fn tar_round_trips_through_gzip() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "hello.txt", "world");
        let out = src.path().join("a.tgz");
        assert_eq!(
            tar(argv(&[
                "tar",
                "-czf",
                &out.to_string_lossy(),
                "-C",
                &src.path().to_string_lossy(),
                "hello.txt",
            ])),
            OK
        );

        let dest = tempfile::tempdir().unwrap();
        assert_eq!(
            tar(argv(&[
                "tar",
                "-xf",
                &out.to_string_lossy(),
                "-C",
                &dest.path().to_string_lossy(),
            ])),
            OK
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn gzip_is_reproducible_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            write(dir.path(), name, "the same content");
            let p = dir.path().join(name);
            assert_eq!(gzip(argv(&["gzip", "-k", &p.to_string_lossy()])), OK);
            std::fs::read(dir.path().join(format!("{name}.gz"))).unwrap()
        };
        // Two files with identical content compress to identical bytes only if
        // the name and mtime stay out of the header.
        assert_eq!(make("one"), make("two"));

        let p = dir.path().join("one.gz");
        assert_eq!(gunzip(argv(&["gunzip", &p.to_string_lossy()])), OK);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one")).unwrap(),
            "the same content"
        );
    }

    #[test]
    fn gzip_removes_its_input_unless_told_to_keep_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "gone", "x");
        let p = dir.path().join("gone");
        assert_eq!(gzip(argv(&["gzip", &p.to_string_lossy()])), OK);
        assert!(!p.exists(), "gzip replaces its input");
        assert!(dir.path().join("gone.gz").exists());
    }

    #[test]
    fn zstd_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "z", "compress me");
        let p = dir.path().join("z");
        assert_eq!(zstd(argv(&["zstd", "-k", &p.to_string_lossy()])), OK);
        let zp = dir.path().join("z.zst");
        assert!(zp.exists());
        assert_eq!(zstd(argv(&["zstd", "-d", "-k", &zp.to_string_lossy()])), OK);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "compress me");
    }

    #[test]
    fn compression_is_detected_from_the_bytes_not_the_name() {
        // A `.tar` that is actually gzipped must still extract: guessing from
        // the name is how you get a confusing failure instead of a file.
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "f", "content");
        let out = src.path().join("misnamed.tar");
        assert_eq!(
            tar(argv(&[
                "tar",
                "-czf",
                &out.to_string_lossy(),
                "-C",
                &src.path().to_string_lossy(),
                "f",
            ])),
            OK
        );
        let dest = tempfile::tempdir().unwrap();
        assert_eq!(
            tar(argv(&[
                "tar",
                "-xf",
                &out.to_string_lossy(),
                "-C",
                &dest.path().to_string_lossy(),
            ])),
            OK
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("f")).unwrap(),
            "content"
        );
    }
}
