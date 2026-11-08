//! An instrument, not a test: what a fresh process pays before its first
//! lookup, phase by phase, on the bench corpus's shape (5,000 rows over 1,000
//! packages, one section line each). `#[ignore]`d; run with
//! `cargo test --release -p walk startup_instrument -- --ignored --nocapture`.

use super::fs::lstat;
use super::index::CodegenIndex;
use super::lookup::LookupMeta;
use super::snapshot::Base;
use super::store::{Row, RowKind};
use super::testfs::{TestFs, record};
use crate::gitignore_section::{BEGIN_MARKER, END_MARKER};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PKGS: usize = 1000;
const FILES: usize = 5000;
const REPS: usize = 15;

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore = "instrument: prints timings"]
fn startup_instrument() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = root.join(".heph");
    std::fs::create_dir_all(&root).unwrap();
    let fs = Arc::new(TestFs::default());

    let mut rels = Vec::with_capacity(FILES);
    for i in 0..FILES {
        let pkg = i % PKGS;
        let rel = format!("pkg{pkg}/gen{}.txt", i / PKGS);
        let p = root.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, rel.as_bytes()).unwrap();
        std::fs::write(root.join(format!("pkg{pkg}/src{}.go", i / PKGS)), b"src").unwrap();
        rels.push(rel);
    }
    rels.sort();
    let mut gi = format!("{BEGIN_MARKER}\n");
    for r in &rels {
        let pkg = r.split_once('/').unwrap().0;
        writeln!(gi, "# //{pkg}:codegen\n/{r}").unwrap();
    }
    gi.push_str(END_MARKER);
    gi.push('\n');
    std::fs::write(root.join(".gitignore"), gi).unwrap();

    {
        let idx = CodegenIndex::open(&root, &home, fs.clone()).unwrap();
        idx.create_db(&super::ImportLockGuard::new(Box::new(())))
            .unwrap();
        let rows: Vec<Row> = rels
            .iter()
            .map(|rel| {
                let abs = root.join(rel);
                let st = lstat(&abs).unwrap();
                let h = crate::file_hashout(&abs, false).unwrap();
                let pkg = rel.split_once('/').unwrap().0;
                let mut r = Row::new(
                    rel.as_bytes().to_vec(),
                    format!("//{pkg}:codegen"),
                    RowKind::File,
                );
                r.output = Some(rel.rsplit('/').next().unwrap().to_owned());
                r.record = Some(record(h, &st, st.mtime_ns + 10_000_000_000));
                r.seen_in_section = true;
                r
            })
            .collect();
        idx.store()
            .unwrap()
            .write(|t| {
                for r in &rows {
                    t.put(r)?;
                }
                Ok(())
            })
            .unwrap();
    }

    let (
        mut open,
        mut load,
        mut base,
        mut section,
        mut first_snap,
        mut gen_lookups,
        mut src_lookups,
    ) = (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    for rep in 0..=REPS {
        // Phases in isolation, each on a fresh open.
        let t = Instant::now();
        let idx = CodegenIndex::open(&root, &home, fs.clone()).unwrap();
        let d_open = t.elapsed();
        let store = idx.store().unwrap();
        let t = Instant::now();
        let (g, rows) = store.load_all().unwrap();
        let d_load = t.elapsed();
        let t = Instant::now();
        let b = Base::from_rows(g, rows);
        let d_base = t.elapsed();
        drop(b);
        let t = Instant::now();
        idx.section().unwrap();
        let d_section = t.elapsed();
        drop(idx);

        // The whole first snapshot of a fresh process, then its lookups.
        let idx = CodegenIndex::open(&root, &home, fs.clone()).unwrap();
        let t = Instant::now();
        let snap = idx.snapshot().unwrap();
        let d_snap = t.elapsed();
        let t = Instant::now();
        for r in &rels {
            let st = snap
                .status(r.as_bytes(), &LookupMeta::walk(false, false))
                .unwrap();
            assert!(st.kind.is_excluded());
        }
        let d_gen = t.elapsed();
        let t = Instant::now();
        for r in &rels {
            let s = r.replace("gen", "src").replace(".txt", ".go");
            let st = snap
                .status(s.as_bytes(), &LookupMeta::walk(false, false))
                .unwrap();
            assert!(!st.kind.is_excluded());
        }
        let d_src = t.elapsed();
        let t = Instant::now();
        for r in &rels {
            std::hint::black_box(snap.section().probe(
                r.as_bytes(),
                false,
                crate::codegen_index::SectionProbe::Walk {
                    parent_territory: false,
                },
            ));
        }
        let d_probe = t.elapsed();
        let t = Instant::now();
        for r in &rels {
            std::hint::black_box(lstat(&root.join(r)).unwrap());
        }
        let d_lstat = t.elapsed();
        let gi_bytes = std::fs::read(root.join(".gitignore")).unwrap();
        let t = Instant::now();
        std::hint::black_box(crate::gitignore_section::parse_section_bytes(&gi_bytes));
        let d_parse = t.elapsed();
        if rep == REPS {
            println!("  gen probes only      {d_probe:?}");
            println!("  gen lstats only      {d_lstat:?}");
            println!("  section parse only   {d_parse:?}");
        }
        if rep == 0 {
            continue; // warmup
        }
        open.push(d_open);
        load.push(d_load);
        base.push(d_base);
        section.push(d_section);
        first_snap.push(d_snap);
        gen_lookups.push(d_gen);
        src_lookups.push(d_src);
    }
    println!("medians over {REPS} reps, {FILES} rows, {FILES}-line section");
    println!("  Store::open          {:?}", median(open));
    println!("  load_all (sqlite)    {:?}", median(load));
    println!("  Base::from_rows      {:?}", median(base));
    println!("  section compile      {:?}", median(section));
    println!("  first snapshot()     {:?}", median(first_snap));
    println!("  {FILES} gen lookups    {:?}", median(gen_lookups));
    println!("  {FILES} src lookups    {:?}", median(src_lookups));
}
