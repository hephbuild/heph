//! The `archive.create` driver: pack the outputs of `deps` into one archive
//! file, in-process, through the reproducible [`harchive::write`]. The
//! user-facing description is `docs/ARCHIVE.md`.
//!
//! **Layout.** An entry's name is the workspace-relative path its file was
//! staged at — the path its producer declared, `pkg/out/bin/tool` — with
//! `strip_prefix` removed and then `prefix` prepended. A file not under
//! `strip_prefix` is an error, not left where it was: a partly-stripped
//! archive is never what was meant.
//!
//! **Reproducible by construction.** Everything the bytes depend on is in the
//! def hash or is the deps' content: the writer sorts the entries, flattens
//! owners and modes, and stamps every tar entry with mtime 0. Nothing is read
//! from the environment — `SOURCE_DATE_EPOCH` is the `tar` coreutil's
//! convention, and read here it would be an input no key covers.

use crate::{WritableFormatArg, check_rel, in_package, read_only, writable_spellings};
use anyhow::Context as _;
use async_trait::async_trait;
use harchive::{Format, Member, WriteOptions};
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunRequest, ManagedRunResponse, dep_inputs, read_listed_paths,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{FromSpecValue as _, Spec, TargetSpecCache};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// The driver's local name; the engine reaches it as `archive.create`.
pub const DRIVER_NAME: &str = "create";

/// Bump to invalidate every archive when what the writer or the layout
/// produces from the same files changes (header fields, compressor settings,
/// entry naming).
const CREATE_FORMAT_VERSION: u32 = 1;

/// The mtime of every tar entry. Fixed: the bytes must be a function of the
/// def and the deps alone.
const MTIME: u64 = 0;

/// Every dep input's origin id. One for all of them: the files are packed
/// together, and nothing distinguishes one dep's from another's but its path.
const DEPS_ORIGIN: &str = "create:deps";

/// Config for a `create` target.
#[derive(Spec)]
struct CreateSpec {
    /// Targets whose outputs are packed (required, at least one). `|group`
    /// and `[filters]` pick which of a target's files.
    #[spec(required)]
    deps: Vec<String>,
    /// Archive format: `tar`, `tar.gz`, `tar.zst` or `zip`. Inferred from
    /// `out`'s suffix when omitted. `tar.xz` and `gz` are read only.
    #[spec(ty = WritableFormatArg::spec_param_type())]
    format: Option<WritableFormatArg>,
    /// Output file, relative to the target's package. Defaults to
    /// `<name>.<format>`.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// Leading directory removed from every entry, written workspace-relative
    /// (`pkg/out`). Every packed file must be under it.
    #[spec(ty = ParamType::String)]
    strip_prefix: Option<String>,
    /// Leading directory prepended to every entry (after `strip_prefix`), e.g.
    /// `tool-1.0`.
    #[spec(ty = ParamType::String)]
    prefix: Option<String>,
    /// Cache the archive locally and remotely (default). It is a pure function
    /// of the deps' bytes and the options above. `cache = False` disables both
    /// tiers; the dict form (`cache = {"remote": False}`) keeps it local-only.
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu (compression is CPU-bound). Never
    /// hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CreateDef {
    format: Format,
    /// Package-relative output file.
    out: String,
    layout: Layout,
}

/// Where a staged file lands in the archive. See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Layout {
    strip_prefix: Option<String>,
    prefix: Option<String>,
}

impl Layout {
    /// The entry name of the file staged at workspace-relative `ws_rel`.
    fn entry_name(&self, ws_rel: &str) -> anyhow::Result<String> {
        let rest = match &self.strip_prefix {
            None => ws_rel,
            Some(strip) => match ws_rel
                .strip_prefix(strip.as_str())
                .map(|r| (r.is_empty(), r.strip_prefix('/')))
            {
                Some((false, Some(rest))) => rest,
                Some((true, _)) => anyhow::bail!(
                    "{ws_rel:?} is `strip_prefix` itself: stripping it leaves no name"
                ),
                // `pkg/outer` is not under `pkg/out`: components, not bytes.
                _ => anyhow::bail!(
                    "{ws_rel:?} is not under `strip_prefix` {strip:?}; every packed file must \
                     be (or narrow `deps` with `|group`/`[filters]`)"
                ),
            },
        };
        Ok(match &self.prefix {
            None => rest.to_string(),
            Some(prefix) => format!("{prefix}/{rest}"),
        })
    }
}

/// `out`, or `<name>.<format>`, and the format: the option, else what `out`'s
/// suffix implies. Both set and disagreeing (`format = "zip"`, `out =
/// "x.tar.gz"`) is refused: whichever won, the file would lie about its
/// content. An `out` whose suffix names no format takes the option, and so
/// does one whose suffix names a format only `archive.extract` reads (`x.gz`,
/// `x.txz`): those named none before `extract` read them, and still parse.
/// With `out` alone, such a suffix is refused as read only.
fn out_and_format(
    name: &str,
    out: Option<String>,
    format: Option<WritableFormatArg>,
) -> anyhow::Result<(String, Format)> {
    match (out, format) {
        (Some(out), Some(WritableFormatArg(f))) => {
            match Format::infer(&out).filter(|named| named.is_writable()) {
                Some(named) if named != f => anyhow::bail!(
                    "`format` is {:?} but `out` {out:?} names a {:?}; make them agree, or drop \
                 `format` to take it from `out`",
                    f.as_str(),
                    named.as_str()
                ),
                _ => Ok((out, f)),
            }
        }
        (None, Some(WritableFormatArg(f))) => Ok((format!("{name}.{}", f.as_str()), f)),
        (Some(out), None) => {
            let f = Format::infer(&out).with_context(|| {
                format!(
                    "`format` is not set and `out` {out:?} names none (a suffix: {}); set \
                     `format`",
                    writable_spellings()
                )
            })?;
            if !f.is_writable() {
                return Err(read_only(f).context(format!("`out` {out:?}")));
            }
            Ok((out, f))
        }
        (None, None) => anyhow::bail!(
            "set `format` ({}), or an `out` whose suffix names one",
            writable_spellings()
        ),
    }
}

pub struct Driver;

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        CreateSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let at = || addr.format();
        let spec = CreateSpec::from(&req.target_spec.config).context("parse create config")?;
        let (out, format) = out_and_format(&addr.name, spec.out, spec.format).with_context(at)?;
        check_rel(&out).with_context(|| format!("{}: `out`", at()))?;
        for (opt, v) in [
            ("strip_prefix", &spec.strip_prefix),
            ("prefix", &spec.prefix),
        ] {
            if let Some(v) = v {
                check_rel(v).with_context(|| format!("{}: `{opt}`", at()))?;
            }
        }
        if spec.deps.is_empty() {
            anyhow::bail!("{}: `deps` is empty: there is nothing to pack", at());
        }

        let pkg = addr.package.clone();
        // Sorted and deduplicated: the archive does not depend on the order
        // the deps are listed in, or on one being listed twice, so neither
        // may move the key.
        let deps: BTreeMap<String, TargetAddr> = spec
            .deps
            .iter()
            .map(|d| {
                let a = TargetAddr::parse(d, &pkg)
                    .with_context(|| format!("{}: parsing dep {d:?}", at()))?;
                Ok((a.to_string(), a))
            })
            .collect::<anyhow::Result<_>>()?;
        let layout = Layout {
            strip_prefix: spec.strip_prefix,
            prefix: spec.prefix,
        };

        let mut h = Xxh3::new();
        h.update(&CREATE_FORMAT_VERSION.to_le_bytes());
        h.update(addr.format().as_bytes());
        h.update(b"\0");
        // The files' bytes reach the key through the deps' hashouts, so only
        // the addresses go in here — all of each: `|group` and `[filters]`
        // pick which of a dep's files are packed.
        h.update(&(deps.len() as u64).to_le_bytes());
        for d in deps.keys() {
            h.update(d.as_bytes());
            h.update(b"\0");
        }
        h.update(format.as_str().as_bytes());
        h.update(b"\0");
        h.update(out.as_bytes());
        h.update(b"\0");
        for opt in [&layout.strip_prefix, &layout.prefix] {
            match opt {
                None => h.update(&[0]),
                Some(v) => {
                    h.update(&[1]);
                    h.update(v.as_bytes());
                    h.update(b"\0");
                }
            }
        }
        h.update(&MTIME.to_le_bytes());
        let hash = format!("{:016x}", h.digest()).into_bytes();

        let out_ws = in_package(pkg.as_str(), &out);
        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(CreateDef {
                    format,
                    out,
                    layout,
                }),
                inputs: deps
                    .into_values()
                    .map(|r#ref| Input {
                        r#ref,
                        mode: InputMode::Standard,
                        origin_id: DEPS_ORIGIN.to_string(),
                        annotations: BTreeMap::new(),
                        hashed: true,
                        runtime: true,
                    })
                    .collect(),
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out_ws),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                hash,
                // A pure function of the deps' bytes (and `hash` above).
                cache: spec.cache.into(),
                // Not in `hash` above: a declaration moves no key.
                resources: spec.resources,
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
        let def = req.request.target.def_de::<CreateDef>().clone();
        let addr = req.request.target.addr.format();
        // Several deps may share one list file (a group expands into inputs
        // with its origin), so each list is read once.
        let lists: BTreeSet<(PathBuf, PathBuf)> = dep_inputs(&req.inputs, DEPS_ORIGIN)
            .map(|m| Ok((m.require_list_path()?.to_path_buf(), m.unpack_root.clone())))
            .collect::<anyhow::Result<_>>()
            .with_context(|| format!("{addr}: `deps`"))?;
        let dest = req.sandbox_pkg_dir.join(&def.out);
        let format = def.format;

        // Reading, compressing and writing is blocking work; off the async
        // workers. The token is polled between entries and on every read, so
        // a cancelled run stops within one chunk.
        let token = ctoken.clone_arc();
        hcore::blocking::run(move || -> anyhow::Result<()> {
            let members = members(&lists, &def.layout)?;
            let file = create_out(&dest, &def.out, &members)?;
            let mut out = harchive::write(
                std::io::BufWriter::new(file),
                format,
                members,
                WriteOptions { mtime: MTIME },
                &|| token.is_cancelled(),
            )?;
            out.flush()
                .with_context(|| format!("write {}", dest.display()))
        })
        .await
        .with_context(|| format!("{addr}: create {}", format.as_str()))?;

        Ok(ManagedRunResponse::default())
    }
}

/// Every staged file the `lists` name, as archive members under `layout`.
/// Each list is a dep input's: absolute paths under its `unpack_root` (the
/// sandbox workspace), which a path is made relative to. A file two lists
/// both name (two deps staging one source file) is packed once.
fn members(lists: &BTreeSet<(PathBuf, PathBuf)>, layout: &Layout) -> anyhow::Result<Vec<Member>> {
    // Workspace-relative path → source, every staged file once. One path
    // staged from two different sources (two roots) is ambiguous and refused.
    let mut staged: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (list, root) in lists {
        for source in read_listed_paths(list)? {
            let ws_rel = source
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .with_context(|| {
                    format!(
                        "staged file {} is not a UTF-8 path under {}",
                        source.display(),
                        root.display()
                    )
                })?
                .to_string();
            match staged.get(&ws_rel) {
                Some(seen) if *seen == source => {}
                Some(seen) => anyhow::bail!(
                    "{} and {} were both staged as {ws_rel:?}",
                    seen.display(),
                    source.display()
                ),
                None => {
                    staged.insert(ws_rel, source);
                }
            }
        }
    }
    if staged.is_empty() {
        anyhow::bail!("the deps produced no files: there is nothing to pack");
    }
    if let Some(strip) = &layout.strip_prefix {
        check_under(strip, staged.keys().map(String::as_str))?;
    }

    // Entry name → source, sorted by name, the order the members are written
    // in. No two paths can land on one name: `staged`'s keys are distinct,
    // and `entry_name` is injective (it strips one fixed prefix from paths
    // that all carry it, and prepends another).
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (ws_rel, source) in staged {
        let name = layout.entry_name(&ws_rel)?;
        let previous = by_name.insert(name, source);
        debug_assert!(
            previous.is_none(),
            "entry_name mapped two staged paths to one name"
        );
    }
    Ok(by_name
        .into_iter()
        .map(|(name, source)| Member {
            name: PathBuf::from(name),
            source,
        })
        .collect())
}

/// Create the archive file at `dest` (`out` in the package), which must not
/// exist. A dep that staged a file there would have it truncated before it is
/// read: refused by name when it is a member, and by `create_new` for anything
/// else already at `out`.
fn create_out(dest: &std::path::Path, out: &str, members: &[Member]) -> anyhow::Result<File> {
    if let Some(m) = members.iter().find(|m| m.source == dest) {
        anyhow::bail!(
            "`out` {out:?} is the path a dep staged entry {} at; set `out` to another file",
            m.name.display()
        );
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    File::create_new(dest).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!(
                "`out` {out:?} already exists in the sandbox: a dep staged that path; set `out` \
                 to another file"
            )
        } else {
            anyhow::Error::new(e).context(format!("create {}", dest.display()))
        }
    })
}

/// How many paths the "not under `strip_prefix`" error lists.
const LISTED: usize = 5;

/// Every one of `staged` (sorted) is under `strip`; if not, the error lists
/// the first few that are not and suggests the deepest directory they all
/// share. Staged paths are workspace-relative (`pkg/out/bin/tool`), which an
/// author rarely has in mind when writing `strip_prefix`, so the error shows
/// them.
fn check_under<'s>(
    strip: &str,
    staged: impl Iterator<Item = &'s str> + Clone,
) -> anyhow::Result<()> {
    let is_under = |p: &str| {
        p.strip_prefix(strip)
            .is_some_and(|r| r.is_empty() || r.starts_with('/'))
    };
    let outside: Vec<&str> = staged.clone().filter(|p| !is_under(p)).collect();
    if outside.is_empty() {
        return Ok(());
    }
    let mut msg = format!(
        "{} of the staged files {} not under `strip_prefix` {strip:?}:",
        outside.len(),
        if outside.len() == 1 { "is" } else { "are" }
    );
    for p in outside.iter().take(LISTED) {
        msg.push_str(&format!("\n  {p}"));
    }
    if outside.len() > LISTED {
        msg.push_str(&format!("\n  … and {} more", outside.len() - LISTED));
    }
    match common_dir(staged) {
        Some(common) => msg.push_str(&format!(
            "\nstaged paths are workspace-relative; did you mean `strip_prefix = {common:?}`? \
             (or narrow `deps` with `|group`/`[filters]`)"
        )),
        None => msg.push_str(
            "\nstaged paths are workspace-relative and share no directory; narrow `deps` with \
             `|group`/`[filters]`, or drop `strip_prefix`",
        ),
    }
    anyhow::bail!(msg)
}

/// The deepest directory every one of `paths` is under, by components:
/// `pkg/out` for `pkg/out/a` and `pkg/out/sub/b`. `None` when they share none.
fn common_dir<'s>(mut paths: impl Iterator<Item = &'s str>) -> Option<String> {
    let parent = |p: &'s str| p.rsplit_once('/').map(|(dir, _)| dir);
    let mut common: Vec<&str> = parent(paths.next()?)?.split('/').collect();
    for p in paths {
        let dir = parent(p)?;
        let shared = common
            .iter()
            .zip(dir.split('/'))
            .take_while(|(a, b)| **a == *b)
            .count();
        common.truncate(shared);
        if common.is_empty() {
            return None;
        }
    }
    Some(common.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    async fn parse(addr: &str, cfg: &[(&str, Value)]) -> anyhow::Result<TargetDef> {
        let config: HashMap<String, Value> = cfg
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        Ok(Driver
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr(addr)?,
                        driver: DRIVER_NAME.to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await?
            .target_def)
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn list(items: &[&str]) -> Value {
        Value::List(items.iter().map(|i| s(i)).collect())
    }

    fn def_of(def: &TargetDef) -> CreateDef {
        // Across the ABI the def travels serialized; read it back that way.
        serde_json::from_value(serde_json::to_value(def.def_de::<CreateDef>()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn deps_are_hashed_inputs_and_out_a_package_file() {
        let def = parse(
            "//pkg:bundle",
            &[
                ("deps", list(&[":b", "//other:a"])),
                ("format", s("tar.gz")),
            ],
        )
        .await
        .unwrap();
        let refs: Vec<String> = def.inputs.iter().map(|i| i.r#ref.r#ref.format()).collect();
        assert_eq!(refs, ["//other:a", "//pkg:b"], "sorted");
        assert!(def.inputs.iter().all(|i| i.hashed));
        assert!(matches!(
            &def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "pkg/bundle.tar.gz"
        ));
        assert!(def.cache.enabled, "cached by default");
    }

    #[tokio::test]
    async fn every_option_moves_the_hash_and_resources_do_not() {
        let base = [("deps", list(&[":a"])), ("format", s("tar"))];
        let hash = |extra: Vec<(&'static str, Value)>| {
            let mut cfg = base.to_vec();
            cfg.extend(extra);
            async move { parse("//pkg:t", &cfg).await.unwrap().hash }
        };
        let plain = hash(vec![]).await;
        for (k, v) in [
            ("format", s("zip")),
            ("out", s("x.tar")),
            ("strip_prefix", s("pkg")),
            ("prefix", s("pkg")),
            ("deps", list(&[":b"])),
            ("deps", list(&[":a", ":b"])),
            ("deps", list(&[":a|bin"])),
            ("deps", list(&[":a[x.txt]"])),
        ] {
            assert_ne!(
                hash(vec![(k, v.clone())]).await,
                plain,
                "{k}={v:?} must reach the def hash"
            );
        }
        // `strip_prefix` and `prefix` of one value are different layouts.
        assert_ne!(
            hash(vec![("strip_prefix", s("pkg"))]).await,
            hash(vec![("prefix", s("pkg"))]).await
        );
        let heavy = hash(vec![(
            "resources",
            Value::Map([("cpu".to_string(), Value::Int(4))].into()),
        )])
        .await;
        assert_eq!(heavy, plain, "resources never reach the hash");
        // The archive does not depend on how the deps were listed.
        assert_eq!(hash(vec![("deps", list(&[":a", ":a"]))]).await, plain);
        assert_eq!(
            hash(vec![("deps", list(&[":b", ":a"]))]).await,
            hash(vec![("deps", list(&[":a", ":b"]))]).await
        );
    }

    #[tokio::test]
    async fn the_group_of_a_dep_reaches_the_hash() {
        let hash = |dep: &'static str| async move {
            parse("//pkg:t", &[("deps", list(&[dep])), ("format", s("tar"))])
                .await
                .unwrap()
                .hash
        };
        assert_ne!(hash(":a|x").await, hash(":a|y").await);
        assert_ne!(hash(":a|x").await, hash(":a").await);
    }

    #[tokio::test]
    async fn the_format_comes_from_the_option_else_from_out() {
        for (cfg, out, format) in [
            (vec![("format", s("zip"))], "pkg/t.zip", Format::Zip),
            (
                vec![("out", s("dist/t.tgz"))],
                "pkg/dist/t.tgz",
                Format::TarGz,
            ),
            (
                vec![("out", s("t.TAR.ZST"))],
                "pkg/t.TAR.ZST",
                Format::TarZst,
            ),
            // An `out` that names no format takes the option.
            (
                vec![("out", s("t.bin")), ("format", s("tar"))],
                "pkg/t.bin",
                Format::Tar,
            ),
            // An `out` whose suffix names only a read-only format names none
            // here, as it did before `archive.extract` read those formats.
            (
                vec![("out", s("x.gz")), ("format", s("tar.gz"))],
                "pkg/x.gz",
                Format::TarGz,
            ),
            (
                vec![("out", s("x.txz")), ("format", s("tar"))],
                "pkg/x.txz",
                Format::Tar,
            ),
            // Both set and agreeing, in another spelling of the suffix.
            (
                vec![("out", s("t.tgz")), ("format", s("tar.gz"))],
                "pkg/t.tgz",
                Format::TarGz,
            ),
        ] {
            let mut full = vec![("deps", list(&[":a"]))];
            full.extend(cfg.clone());
            let def = parse("//pkg:t", &full).await.unwrap();
            assert!(
                matches!(&def.outputs[0].paths[0].content, Content::FilePath(p) if p == out),
                "{cfg:?}"
            );
            assert_eq!(def_of(&def).format, format, "{cfg:?}");
        }
    }

    #[tokio::test]
    async fn bad_options_are_refused_at_parse() {
        let a = || ("deps", list(&[":a"]));
        for (cfg, want) in [
            (vec![a()], "set `format`"),
            (vec![a(), ("out", s("t.rar"))], "names none"),
            (vec![a(), ("format", s("rar"))], "unknown archive format"),
            // The formats only `extract` reads, named or implied by `out`.
            (
                vec![a(), ("format", s("tar.xz"))],
                "archive.create cannot write a \"tar.xz\"",
            ),
            (
                vec![a(), ("format", s("gz"))],
                "archive.create cannot write a \"gz\"",
            ),
            (
                vec![a(), ("out", s("t.tar.xz"))],
                "cannot write a \"tar.xz\"",
            ),
            (vec![a(), ("out", s("t.gz"))], "cannot write a \"gz\""),
            // `format` and `out`'s suffix disagree: the file would lie.
            (
                vec![a(), ("format", s("zip")), ("out", s("x.tar.gz"))],
                "`format` is \"zip\" but `out` \"x.tar.gz\" names a \"tar.gz\"",
            ),
            (
                vec![a(), ("format", s("tar")), ("out", s("x.TGZ"))],
                "names a \"tar.gz\"",
            ),
            (
                vec![a(), ("format", s("tar.gz")), ("out", s("x.tar"))],
                "names a \"tar\"",
            ),
            (vec![("format", s("tar"))], "deps"),
            (
                vec![("deps", list(&[])), ("format", s("tar"))],
                "nothing to pack",
            ),
            (vec![a(), ("out", s("../t.tar"))], "`..`"),
            (vec![a(), ("out", s("/t.tar"))], "relative"),
            (
                vec![a(), ("format", s("tar")), ("strip_prefix", s("pkg/"))],
                "strip_prefix",
            ),
            (
                vec![a(), ("format", s("tar")), ("prefix", s("../up"))],
                "prefix",
            ),
            (
                vec![a(), ("format", s("tar")), ("prefix", s(""))],
                "must not be empty",
            ),
        ] {
            let Err(e) = parse("//pkg:t", &cfg).await else {
                panic!("{cfg:?} must be refused")
            };
            let msg = format!("{e:#}");
            assert!(msg.contains(want), "{cfg:?}: {msg}");
        }
    }

    /// A `strip_prefix` the staged files are not under says which files and
    /// what would have worked: authors think of the output dir, the files
    /// are staged workspace-relative.
    #[test]
    fn a_wrong_strip_prefix_lists_the_files_and_suggests_one() {
        let staged = [
            "pkg/out/bin/a",
            "pkg/out/bin/b",
            "pkg/out/lib/c",
            "pkg/out/lib/d",
            "pkg/out/lib/e",
            "pkg/out/lib/f",
            "pkg/out/share/g",
        ];
        let msg = format!(
            "{:#}",
            check_under("out", staged.iter().copied()).unwrap_err()
        );
        assert!(
            msg.starts_with("7 of the staged files are not under `strip_prefix` \"out\":"),
            "{msg}"
        );
        for listed in &staged[..5] {
            assert!(msg.contains(&format!("\n  {listed}\n")), "{listed}: {msg}");
        }
        assert!(
            !msg.contains("pkg/out/lib/f\n"),
            "only the first five: {msg}"
        );
        assert!(msg.contains("… and 2 more"), "{msg}");
        assert!(
            msg.contains("did you mean `strip_prefix = \"pkg/out\"`?"),
            "{msg}"
        );

        // Some under, some not: only the strays are listed, and the
        // suggestion is what all of them share.
        let msg = format!(
            "{:#}",
            check_under("pkg/out/bin", staged.iter().copied()).unwrap_err()
        );
        assert!(
            msg.starts_with("5 of the staged files are not under"),
            "{msg}"
        );
        assert!(!msg.contains("\n  pkg/out/bin/a"), "{msg}");
        assert!(msg.contains("`strip_prefix = \"pkg/out\"`"), "{msg}");

        // Nothing shared: no suggestion to make.
        let msg = format!(
            "{:#}",
            check_under("pkg", ["pkg/a", "other/b"].into_iter()).unwrap_err()
        );
        assert!(msg.contains("share no directory"), "{msg}");
        assert!(!msg.contains("did you mean"), "{msg}");

        // A byte-prefix near miss is not under; all under is fine.
        assert!(check_under("pkg/out", ["pkg/outer/a"].into_iter()).is_err());
        check_under("pkg/out", staged.iter().copied()).unwrap();
    }

    /// `out` never overwrites a dep's staged file: the archive would be
    /// written over a member before the member is read.
    #[test]
    fn out_never_overwrites_a_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("pkg/t.tar");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, "dep's bytes").unwrap();
        let member = Member {
            name: PathBuf::from("pkg/t.tar"),
            source: staged.clone(),
        };

        let msg = format!(
            "{:#}",
            create_out(&staged, "t.tar", std::slice::from_ref(&member)).unwrap_err()
        );
        assert!(
            msg.contains("`out` \"t.tar\" is the path a dep staged entry pkg/t.tar at"),
            "{msg}"
        );
        // Staged there but not packed (filtered out): still not overwritten.
        let msg = format!("{:#}", create_out(&staged, "t.tar", &[]).unwrap_err());
        assert!(msg.contains("a dep staged that path"), "{msg}");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "dep's bytes");

        // A fresh path, in a directory that does not exist yet.
        let fresh = dir.path().join("pkg/dist/t.tar");
        create_out(&fresh, "dist/t.tar", &[member]).unwrap();
        assert!(fresh.is_file());
    }

    #[test]
    fn common_dir_is_by_components() {
        for (paths, want) in [
            (&["pkg/out/a"][..], Some("pkg/out")),
            (&["pkg/out/a", "pkg/out/sub/b"][..], Some("pkg/out")),
            (&["pkg/outer/a", "pkg/out/b"][..], Some("pkg")),
            (&["a/x", "b/y"][..], None),
            (&["top"][..], None),
            (&["pkg/a", "top"][..], None),
            (&[][..], None),
        ] {
            assert_eq!(
                common_dir(paths.iter().copied()).as_deref(),
                want,
                "{paths:?}"
            );
        }
    }

    #[test]
    fn layout_strips_then_prefixes() {
        let layout = |strip: Option<&str>, prefix: Option<&str>| Layout {
            strip_prefix: strip.map(str::to_string),
            prefix: prefix.map(str::to_string),
        };
        let name = |l: &Layout, p: &str| l.entry_name(p);
        assert_eq!(name(&layout(None, None), "pkg/out/a").unwrap(), "pkg/out/a");
        assert_eq!(
            name(&layout(Some("pkg/out"), None), "pkg/out/a").unwrap(),
            "a"
        );
        assert_eq!(
            name(&layout(Some("pkg/out"), Some("tool-1.0")), "pkg/out/bin/t").unwrap(),
            "tool-1.0/bin/t"
        );
        assert_eq!(name(&layout(None, Some("p")), "pkg/a").unwrap(), "p/pkg/a");
        // Outside the prefix, on a byte-prefix near miss, and the prefix itself.
        for (path, want) in [
            ("other/a", "not under"),
            ("pkg/outer/a", "not under"),
            ("pkg/out", "itself"),
        ] {
            let msg = format!(
                "{:#}",
                name(&layout(Some("pkg/out"), None), path).unwrap_err()
            );
            assert!(msg.contains(want), "{path}: {msg}");
        }
    }

    /// The run's half of the layout, on list files as the sandbox writes them.
    #[test]
    fn members_come_from_every_list_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let write_list = |name: &str, rels: &[&str]| {
            let body: String = rels
                .iter()
                .map(|r| format!("{}\n", root.join(r).display()))
                .collect();
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            (p, root.clone())
        };
        let lists: BTreeSet<_> = [
            write_list("one.list", &["pkg/out/b", "pkg/out/a"]),
            // The same file staged by a second dep is packed once.
            write_list("two.list", &["pkg/out/a", "pkg/out/sub/c"]),
        ]
        .into();
        let strip = Layout {
            strip_prefix: Some("pkg/out".to_string()),
            prefix: None,
        };
        let got: Vec<(String, PathBuf)> = members(&lists, &strip)
            .unwrap()
            .into_iter()
            .map(|m| (m.name.to_string_lossy().into_owned(), m.source))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), root.join("pkg/out/a")),
                ("b".to_string(), root.join("pkg/out/b")),
                ("sub/c".to_string(), root.join("pkg/out/sub/c")),
            ]
        );

        let outside: BTreeSet<_> = [write_list("three.list", &["pkg/out/a", "pkg/x"])].into();
        let msg = format!("{:#}", members(&outside, &strip).unwrap_err());
        assert!(msg.contains("1 of the staged files is not under"), "{msg}");
        assert!(msg.contains("\n  pkg/x"), "{msg}");

        let empty: BTreeSet<_> = [write_list("four.list", &[])].into();
        let msg = format!("{:#}", members(&empty, &Layout::default()).unwrap_err());
        assert!(msg.contains("nothing to pack"), "{msg}");
    }

    /// Two lists under different roots that stage the same workspace-relative
    /// path from different sources: which one is the entry is ambiguous, so
    /// it is refused, naming both sources and the path.
    #[test]
    fn one_path_staged_from_two_sources_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let write_list = |name: &str, root: &str, rel: &str| {
            let root = dir.path().join(root);
            let p = dir.path().join(name);
            std::fs::write(&p, format!("{}\n", root.join(rel).display())).unwrap();
            (p, root)
        };
        let lists: BTreeSet<_> = [
            write_list("one.list", "ws1", "pkg/out/a"),
            write_list("two.list", "ws2", "pkg/out/a"),
        ]
        .into();
        let msg = format!("{:#}", members(&lists, &Layout::default()).unwrap_err());
        assert!(msg.contains("were both staged as \"pkg/out/a\""), "{msg}");
        let first = dir.path().join("ws1/pkg/out/a");
        let second = dir.path().join("ws2/pkg/out/a");
        assert!(msg.contains(&first.display().to_string()), "{msg}");
        assert!(msg.contains(&second.display().to_string()), "{msg}");
    }
}
