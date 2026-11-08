//! The codegen-index checks with exact bars (spec: Tests, Bench): what the
//! candidate's own engine must do on the codegen corpus, asserted rather than
//! timed against a baseline, so they run on the candidate alone.
//!
//! - **Warm build**: rebuilding every `copy` target after a full build commits
//!   nothing (the index db's `generation` does not move) and takes no codegen
//!   lock (no file appears under `<home>/lock/codegen`).
//! - **Snapshot publish**: a write-back copies the overlay only, never the
//!   base. A write-back publishes twice (pending, then settled), and each
//!   publish clones the overlay as it stands, so over a loaded base two
//!   write-backs of kA and kB rows clone exactly 3 kA + kB entries. The spec's
//!   "one target's rows per write-back" holds for the first write-back only:
//!   the overlay accumulates until it folds at `FOLD_THRESHOLD`.
//! - **`status` on a miss**: a microbench with 0, 100 and 5,000 section lines,
//!   interleaved; the median at 5,000 lines must be within 1.5 x the 0-line
//!   median.
//!
//! Their timed counterparts (glob, warm build, import) are Tier B scenarios
//! in `dist.rs`, compared against N-1 by `compare`.

use crate::inprocess;
use anyhow::{Context, Result, bail, ensure};
use bench_corpus::{CODEGEN_BEGIN_MARKER, CODEGEN_END_MARKER, CODEGEN_LABEL, CorpusManifest};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::htwalk::codegen_index::{CodegenLockPaths, LookupMeta};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Section sizes the `status` microbench measures.
pub const STATUS_LINES: [usize; 3] = [0, 100, 5_000];
/// The bar: the 5,000-line median over the 0-line median.
pub const STATUS_MAX_RATIO: f64 = 1.5;

/// One line of the report: what was checked, what was seen, and whether it
/// held.
#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub detail: String,
    pub ok: bool,
}

/// Run every check on `corpus` (a codegen corpus: `--codegen-files` > 0).
/// Wipes the corpus's heph home first; restores every BUILD file it edits.
pub async fn check(
    corpus: &Path,
    manifest: &CorpusManifest,
    status_iters: usize,
    status_rounds: usize,
) -> Result<Vec<Check>> {
    ensure!(
        manifest.codegen_addrs.len() >= 2,
        "corpus has {} codegen targets; generate it with --codegen-files and at least 2 packages",
        manifest.codegen_addrs.len()
    );
    let mut out = Vec::new();
    crate::dist::write_codegen_config(corpus)?;
    inprocess::wipe_cache(corpus)?;
    out.push(warm_build(corpus, manifest).await?);
    out.push(publish(corpus, manifest).await?);
    out.push(status_miss(status_iters, status_rounds).await?);
    Ok(out)
}

async fn build(e: &Arc<Engine>, matcher: &Matcher) -> Result<()> {
    let batch = Arc::clone(e)
        .result(
            e.new_state(),
            matcher,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .context("resolve codegen targets")?;
    if let Some((addr, err)) = batch.errors.first() {
        bail!("{}: {err:#}", addr.format());
    }
    Ok(())
}

fn every_copy_target() -> Matcher {
    Matcher::And(vec![
        Matcher::Label(CODEGEN_LABEL.to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ])
}

fn generation(e: &Engine) -> Result<u64> {
    let index = e.codegen_index();
    ensure!(
        index.open_existing()?,
        "no codegen index db at {} after building the copy targets",
        index.db_path().display()
    );
    index
        .store()
        .context("codegen index db not open")?
        .generation()
}

fn lock_files(dir: &Path) -> Result<usize> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.count()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e).with_context(|| format!("list {}", dir.display())),
    }
}

/// A full build, then a warm one in a fresh engine (a new process's view):
/// the warm one commits nothing and creates no lock file.
async fn warm_build(corpus: &Path, manifest: &CorpusManifest) -> Result<Check> {
    let all = every_copy_target();
    let first = inprocess::build_engine(corpus)?;
    build(&first, &all).await.context("first build")?;
    let before = generation(&first)?;
    drop(first);

    let home = inprocess::cache_dir(corpus)?;
    let locks = CodegenLockPaths::new(&home);
    if locks.dir().exists() {
        std::fs::remove_dir_all(locks.dir())
            .with_context(|| format!("remove {}", locks.dir().display()))?;
    }

    let warm = inprocess::build_engine(corpus)?;
    let start = Instant::now();
    build(&warm, &all).await.context("warm build")?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let after = generation(&warm)?;
    let created = lock_files(locks.dir())?;
    Ok(Check {
        name: "warm-build",
        detail: format!(
            "{} copy targets in {ms:.1}ms: generation {before} -> {after}, {created} codegen lock file(s) created (bar: 0 commits, 0 lock files)",
            manifest.codegen_addrs.len()
        ),
        ok: before == after && created == 0,
    })
}

fn package_of(addr: &str) -> Result<&str> {
    addr.strip_prefix("//")
        .and_then(|a| a.split_once(':'))
        .map(|(pkg, _)| pkg)
        .with_context(|| format!("not a //pkg:name addr: {addr}"))
}

/// Over a base holding every row, write back target A then target B (both
/// retagged so each must commit): the publishes clone A's rows and nothing
/// else.
async fn publish(corpus: &Path, manifest: &CorpusManifest) -> Result<Check> {
    let (a, b) = match manifest.codegen_addrs.as_slice() {
        [a, b, ..] => (a.as_str(), b.as_str()),
        _ => bail!("need two codegen targets"),
    };
    let (pkg_a, pkg_b) = (package_of(a)?, package_of(b)?);
    let rows_a = manifest
        .codegen_outputs
        .iter()
        .filter(|p| p.strip_prefix(pkg_a).is_some_and(|r| r.starts_with('/')))
        .count();

    let orig_a = bench_corpus::retag_codegen(corpus, pkg_a, "republished ")?;
    let orig_b = match bench_corpus::retag_codegen(corpus, pkg_b, "republished ") {
        Ok(src) => src,
        Err(e) => {
            restore(corpus, pkg_a, &orig_a)?;
            return Err(e);
        }
    };
    let measured = publish_two(corpus, a, b).await;
    restore(corpus, pkg_a, &orig_a)?;
    restore(corpus, pkg_b, &orig_b)?;
    let (stats_before, stats) = measured?;

    let rows_b = manifest
        .codegen_outputs
        .iter()
        .filter(|p| p.strip_prefix(pkg_b).is_some_and(|r| r.starts_with('/')))
        .count();
    // A write-back publishes twice (rows marked pending, then settled), each
    // cloning the overlay as it stands: A 0 then kA, B kA then kA + kB.
    let expected = u64::try_from(3 * rows_a + rows_b).context("row count")?;
    let cloned = stats.cloned_overlay_entries - stats_before.cloned_overlay_entries;
    Ok(Check {
        name: "snapshot-publish",
        detail: format!(
            "two write-backs ({rows_a} and {rows_b} rows) over a {}-row base cloned {cloned} overlay entries, {} fold(s) (bar: exactly {expected}, the overlay at each of the 4 publishes, never the base)",
            stats.rows_loaded,
            stats.folds - stats_before.folds,
        ),
        ok: cloned == expected && stats.folds == stats_before.folds,
    })
}

async fn publish_two(
    corpus: &Path,
    a: &str,
    b: &str,
) -> Result<(
    heph::htwalk::codegen_index::IndexStats,
    heph::htwalk::codegen_index::IndexStats,
)> {
    let e = inprocess::build_engine(corpus)?;
    let rs = e.new_state();
    e.open_codegen_index(&rs).await?;
    // Load the base before the first write-back, so it is in place when the
    // publishes run and its rows are visible in `rows_loaded`.
    drop(e.codegen_index().snapshot()?);
    let before = e.codegen_index().stats();
    build(&e, &Matcher::Addr(parse_addr(a)?))
        .await
        .with_context(|| format!("write back {a}"))?;
    build(&e, &Matcher::Addr(parse_addr(b)?))
        .await
        .with_context(|| format!("write back {b}"))?;
    Ok((before, e.codegen_index().stats()))
}

fn restore(corpus: &Path, pkg: &str, src: &str) -> Result<()> {
    let path = corpus.join(pkg).join("BUILD");
    std::fs::write(&path, src).with_context(|| format!("restore {}", path.display()))
}

/// A root `.gitignore` holding only a managed section of `lines` patterns,
/// shaped like the corpus's (an attribution line above each).
fn section_of(lines: usize) -> Result<String> {
    let mut s = String::with_capacity(lines * 48 + 128);
    writeln!(s, "{CODEGEN_BEGIN_MARKER}").context("format section")?;
    for i in 0..lines {
        let pkg = i / 5;
        writeln!(s, "# //pkg{pkg}:codegen\n/pkg{pkg}/gen{}.txt", i % 5)
            .context("format section")?;
    }
    writeln!(s, "{CODEGEN_END_MARKER}").context("format section")?;
    Ok(s)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v.get(n / 2).copied().unwrap_or(0.0),
        n => {
            (v.get(n / 2 - 1).copied().unwrap_or(0.0) + v.get(n / 2).copied().unwrap_or(0.0)) / 2.0
        }
    }
}

/// `Snapshot::status` of paths no row records and no section line matches
/// (a walk's common case), over a section of 0, 100 and 5,000 lines. No db,
/// so no fold map: the miss is the row probe plus the section test, and the
/// bar is on how the section test scales.
async fn status_miss(iters: usize, rounds: usize) -> Result<Check> {
    ensure!(
        iters > 0 && rounds > 0,
        "status microbench needs iterations"
    );
    let misses: Vec<Vec<u8>> = (0..1024)
        .map(|i| format!("src/pkg{}/file{i}.go", i % 97).into_bytes())
        .collect();
    let mut dirs = Vec::with_capacity(STATUS_LINES.len());
    let mut snaps = Vec::with_capacity(STATUS_LINES.len());
    for lines in STATUS_LINES {
        let dir = tempfile::tempdir().context("tempdir")?;
        std::fs::write(dir.path().join(".gitignore"), section_of(lines)?)
            .context("write .gitignore")?;
        crate::dist::write_codegen_config(dir.path())?;
        let e = Engine::new(Config::new(
            dir.path().to_path_buf(),
            inprocess::cache_dir(dir.path())?,
        ))
        .context("construct engine")?;
        let snap = e.codegen_index().snapshot()?;
        ensure!(
            snap.section().is_present(),
            "{lines}-line section not found"
        );
        snaps.push(snap);
        dirs.push((dir, e));
    }

    let meta = LookupMeta::point(false);
    let mut per_ns: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); STATUS_LINES.len()];
    for _ in 0..rounds {
        for (snap, samples) in snaps.iter().zip(per_ns.iter_mut()) {
            let start = Instant::now();
            for i in 0..iters {
                let rel = misses.get(i % misses.len()).context("miss path")?;
                let st = snap.status(rel, &meta)?;
                black_box(st.kind);
            }
            samples.push(start.elapsed().as_secs_f64() * 1e9 / iters as f64);
        }
    }
    let medians: Vec<f64> = per_ns.iter_mut().map(|v| median(v)).collect();
    let (zero, top) = match medians.as_slice() {
        [zero, .., top] => (*zero, *top),
        _ => bail!("no samples"),
    };
    let ratio = if zero > 0.0 {
        top / zero
    } else {
        f64::INFINITY
    };
    let per_size: Vec<String> = STATUS_LINES
        .iter()
        .zip(&medians)
        .map(|(lines, ns)| format!("{lines} lines {ns:.1}"))
        .collect();
    let detail = format!(
        "median ns per miss: {}; 5000/0 = {ratio:.2}x (bar: <= {STATUS_MAX_RATIO}x; {rounds} interleaved rounds of {iters})",
        per_size.join(", ")
    );
    Ok(Check {
        name: "status-miss",
        detail,
        ok: ratio <= STATUS_MAX_RATIO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus generator copies the section markers rather than linking
    /// walk; a drift here would make every codegen bench read a stale section.
    #[test]
    fn corpus_markers_are_walks() {
        use heph::htwalk::gitignore_section::{BEGIN_MARKER, END_MARKER};
        assert_eq!(CODEGEN_BEGIN_MARKER, BEGIN_MARKER);
        assert_eq!(CODEGEN_END_MARKER, END_MARKER);
    }

    #[test]
    fn package_of_reads_the_package() {
        assert_eq!(package_of("//pkg12:codegen").ok(), Some("pkg12"));
        package_of("pkg12:codegen").expect_err("no leading //");
    }

    #[test]
    fn median_of_even_and_odd() {
        assert!((median(&mut [3.0, 1.0, 2.0]) - 2.0).abs() < f64::EPSILON);
        assert!((median(&mut [4.0, 1.0, 3.0, 2.0]) - 2.5).abs() < f64::EPSILON);
    }

    /// The microbench's section is one the index finds and whose lines it
    /// matches: a miss path is `Source`, a listed path is territory.
    #[tokio::test]
    async fn status_section_is_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".gitignore"),
            section_of(100).expect("section"),
        )
        .expect("write");
        crate::dist::write_codegen_config(dir.path()).expect("config");
        let e = Engine::new(Config::new(
            dir.path().to_path_buf(),
            inprocess::cache_dir(dir.path()).expect("home"),
        ))
        .expect("engine");
        let snap = e.codegen_index().snapshot().expect("snapshot");
        let meta = LookupMeta::point(false);
        assert!(
            snap.status(b"pkg3/gen2.txt", &meta)
                .expect("status")
                .matched
                .is_territory()
        );
        assert!(
            !snap
                .status(b"src/pkg3/file1.go", &meta)
                .expect("status")
                .matched
                .is_territory()
        );
    }
}
