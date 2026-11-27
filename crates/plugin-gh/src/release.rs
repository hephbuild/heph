//! `heph.gh.release_asset(...)`: a GitHub release asset as a target.
//!
//! The function does no HTTP of its own. It **declares** targets for the
//! calling package: an `http_fetch` of the asset's download URL, and — with
//! `extract = True` — an `archive.extract` (the builtin `archive` plugin's
//! driver, `docs/ARCHIVE.md`) that unpacks it. Fetching, pinning
//! (`sha256`) and caching are `http_fetch`'s, exactly as if the BUILD file had
//! written the `target(...)` calls. See `docs/GH_RELEASES.md`.
//!
//! With `credentials` (a private repository) the asset cannot be fetched from
//! its download URL, which takes no token: it is fetched from the API by id,
//! and the id is only known at run. So the function declares three targets,
//! and the network stays in `http_fetch`:
//!
//! 1. `<name>_release`: `http_fetch` of the release JSON
//!    (`GET {api_url}/repos/{repo}/releases/tags/{tag}`), with the credentials,
//!    `unpinned`, and cached locally only by default (see [`RELEASE_SUFFIX`]);
//! 2. `<name>_url`: `<plugin>.asset_url` ([`crate::asset_url`]), the asset's
//!    API URL picked out of that JSON;
//! 3. the asset: `http_fetch` with `url = "${read://<pkg>:<name>_url}"`,
//!    `Accept: application/octet-stream` and the credentials.
//!
//! Like every plugin function it is pure (`hplugin::function::PluginFn`): the
//! outcome depends on the arguments and the package, never on the network,
//! the environment or the plugin's options. The server and its API are
//! arguments (`server_url`, `api_url`), not options: one workspace may pull
//! from a GitHub Enterprise server and from github.com.

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use harchive::Format;
use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, ParamType};
use hplugin::function::{
    DeclaredTarget, FnArgs, FnCallContext, FnOutcome, PluginFn, PluginFnDef, spec_params,
    spec_params_doc,
};
use hplugin::htspec::{FromSpecValue, Spec, TargetSpecCache};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

/// Where releases are downloaded from when `server_url` is not passed.
pub const DEFAULT_SERVER_URL: &str = "https://github.com";

/// The API of [`DEFAULT_SERVER_URL`]. Any other server (GitHub Enterprise)
/// serves its API at `{server_url}/api/v3`.
pub const GITHUB_API_URL: &str = "https://api.github.com";

/// Suffix of the release-JSON fetch declared with `credentials`.
///
/// That fetch is cached by its URL and headers, so a warm build makes no API
/// call. It is `unpinned = True`: the JSON carries download counts, so no
/// `sha256` can be written for it, and `http_fetch` would otherwise warn about
/// it.
///
/// It is cached **locally only** unless the call's `cache` is a dict that says
/// `"remote": True`. Otherwise it takes the call's `enabled` and `history` and
/// stays local (`cache = False` still turns it off). The JSON holds more than the asset
/// it leads to: the release notes, every asset's name, size and id, and the
/// uploaders' logins. In the remote cache, anyone who can read that cache can
/// read those.
///
/// Its key is the tag, not the bytes: a cached copy keeps naming the asset id
/// it was fetched with. An asset uploaded again under the same name gets a new
/// id, so the asset fetch 404s; an asset uploaded after the JSON was cached is
/// missing from it, so `<name>_url` fails. Both errors say
/// `heph run --force //<pkg>:<name>_release` (the asset fetch's through its
/// `refresh`), which refetches it and overwrites the cached copy: in the remote
/// cache too from a machine that writes the remote cache; a read-only client
/// refreshes only its own.
pub const RELEASE_SUFFIX: &str = "_release";

/// Suffix of the `asset_url` target declared with `credentials`.
pub const URL_SUFFIX: &str = "_url";

/// The API version the release lookup asks for.
const GITHUB_API_VERSION: &str = "2022-11-28";

/// The builtin driver the asset is fetched with. Named, not imported: the gh
/// plugin is a cdylib and `http_fetch` lives in the host.
const HTTP_FETCH: &str = "http_fetch";

/// The builtin driver an archive is unpacked with (the `archive` plugin's, see
/// `docs/ARCHIVE.md`). Named, not imported, for the same reason: linking it
/// would put the unpacker and its codecs into the gh cdylib.
const ARCHIVE_EXTRACT: &str = "archive.extract";

/// Suffix of the fetch target's name when the asset is extracted: the
/// extracted tree takes `name`, the archive `<name>_archive`.
pub const ARCHIVE_SUFFIX: &str = "_archive";

/// The server one `release_asset` call downloads from, and its API, from the
/// call's `server_url` and `api_url` arguments.
#[derive(Debug, Clone)]
pub struct ReleaseConfig {
    /// Base URL of the GitHub server, no trailing `/`. `https://github.com`,
    /// or a GitHub Enterprise host.
    server_url: String,
    /// Base URL of its REST API, no trailing `/`: where a private asset is
    /// looked up and fetched (with `credentials`).
    api_url: String,
}

impl ReleaseConfig {
    /// A config downloading from `server_url`, its API at `api_url` or, when
    /// `None`, where GitHub serves it: [`GITHUB_API_URL`] for `github.com`,
    /// `{server_url}/api/v3` for any other server (GitHub Enterprise). An
    /// error names the argument at fault.
    pub fn new(server_url: &str, api_url: Option<&str>) -> anyhow::Result<Self> {
        let server_url = base_url("server_url", server_url)?;
        // github.com is https only, and its API is not under it: an http
        // spelling would download over a redirect and look the API up at
        // `http://github.com/api/v3`, which does not exist.
        if let Some(("http", host)) = scheme_and_host(&server_url)
            && host.eq_ignore_ascii_case("github.com")
        {
            bail!(
                "`server_url` {server_url:?}: github.com is served over https; write \
                 https://github.com"
            );
        }
        let api_url = match api_url {
            Some(api) => base_url("api_url", api)?,
            None if server_url.eq_ignore_ascii_case(DEFAULT_SERVER_URL) => {
                GITHUB_API_URL.to_string()
            }
            None => format!("{server_url}/api/v3"),
        };
        Ok(Self {
            server_url,
            api_url,
        })
    }
}

/// The scheme and host (no port) of a URL [`base_url`] accepted.
fn scheme_and_host(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = match authority.rsplit_once(':') {
        // `[::1]` is all host; `[::1]:80` and `h:80` end in a port.
        Some((host, port)) if !port.contains(']') => host,
        _ => authority,
    };
    Some((scheme, host))
}

/// A base URL argument, trimmed of trailing `/`s: a scheme, a host and an
/// optional path, nothing a path could not be appended to. An error names the
/// argument, `arg`.
fn base_url(arg: &str, url: &str) -> anyhow::Result<String> {
    // Before anything echoes the URL: a `user:password@` in it is a secret,
    // and this URL is written into every target's config and log line.
    let authority = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        bail!(
            "`{arg}` must not carry a user or password (`user:password@host`): it would be \
             written into every URL and log; pass a token with `credentials =`"
        );
    }
    let trimmed = url.trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .with_context(|| format!("`{arg}` {url:?} must start with https:// or http://"))?;
    if authority.is_empty()
        || authority.starts_with(':')
        || rest
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#'))
    {
        bail!(
            "`{arg}` {url:?} must be a scheme and host (and optional path), with no query, \
             fragment or whitespace"
        );
    }
    Ok(trimmed.to_string())
}

/// Every character but RFC 3986's unreserved set is percent-encoded in a path
/// segment, so a tag or asset name reaches the server as written whatever it
/// contains.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `owner/name`: exactly one `/`, each side GitHub's own charset.
fn check_repo(repo: &str) -> anyhow::Result<()> {
    let valid_side = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid_side(owner) && valid_side(name) => Ok(()),
        _ => bail!("`repo` must be `owner/name` (letters, digits, `.`, `_` and `-`), got {repo:?}"),
    }
}

/// A path segment from the user: non-empty, not `.`/`..` (which a URL parser
/// would resolve away, fetching another path), no control characters.
fn check_segment(what: &str, seg: &str, whole: &str) -> anyhow::Result<()> {
    if seg.is_empty() || seg == "." || seg == ".." {
        bail!("`{what}` {whole:?} has an empty, `.` or `..` path segment");
    }
    if seg.chars().any(char::is_control) {
        bail!("`{what}` {whole:?} contains a control character");
    }
    Ok(())
}

/// The asset's download URL, `{server_url}/{repo}/releases/download/{tag}/{asset}`,
/// escaped for `http_fetch`'s `url` template.
///
/// A tag may contain `/` (`release/v1`): GitHub's own download URL keeps it as
/// a path separator, so the tag is encoded segment by segment and the `/`s are
/// kept. An asset name never contains one, so a `/` there is refused.
///
/// `http_fetch` substitutes `{arg}` placeholders from the target's address
/// args; every brace is doubled so the URL is taken literally. (Percent-encoding
/// already removes them from the tag and asset; the doubling covers `server_url`.)
pub fn asset_url(
    cfg: &ReleaseConfig,
    repo: &str,
    tag: &str,
    asset: &str,
) -> anyhow::Result<String> {
    check_release(repo, tag, asset)?;
    let tag = tag
        .split('/')
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/");
    let url = format!(
        "{}/{repo}/releases/download/{tag}/{}",
        cfg.server_url,
        utf8_percent_encode(asset, SEGMENT)
    );
    Ok(url.replace('{', "{{").replace('}', "}}"))
}

/// The arguments naming an asset, as both URLs need them: `repo` is
/// `owner/name`, no `tag` segment is empty, `.` or `..`, and `asset` is one
/// file name.
fn check_release(repo: &str, tag: &str, asset: &str) -> anyhow::Result<()> {
    check_repo(repo)?;
    for seg in tag.split('/') {
        check_segment("tag", seg, tag)?;
    }
    if asset.contains('/') {
        bail!("`asset` {asset:?} must be a file name, without `/`");
    }
    check_segment("asset", asset, asset)
}

/// The release lookup, `{api_url}/repos/{repo}/releases/tags/{tag}`, escaped for
/// `http_fetch`'s `url` template.
///
/// Unlike the download URL, the tag is **one** encoded segment, its `/`s as
/// `%2F`: in the API it is a single path parameter (`release/v1` is the tag
/// `release/v1`, not a tag `release` followed by more path), and that is how
/// GitHub's own clients send it. The segments are still checked as the
/// download URL's are, so the same tags are accepted by both.
pub fn release_json_url(
    cfg: &ReleaseConfig,
    repo: &str,
    tag: &str,
    asset: &str,
) -> anyhow::Result<String> {
    check_release(repo, tag, asset)?;
    let url = format!(
        "{}/repos/{repo}/releases/tags/{}",
        cfg.api_url,
        utf8_percent_encode(tag, SEGMENT)
    );
    Ok(url.replace('{', "{{").replace('}', "}}"))
}

/// The BUILD-file functions of the release half, for `plugin`'s name. They
/// read no plugin option: everything a call depends on is its arguments.
pub fn functions(plugin: &str) -> Vec<PluginFnDef> {
    let mut named = spec_params::<ReleaseAssetArgs>();
    named.sort_by_key(|p| p.name);
    vec![PluginFnDef {
        name: "release_asset".to_string(),
        signature: FnSignature {
            positional: vec![],
            named,
            variadic: None,
            returns: ParamType::String,
        },
        doc: format!(
            "Declare a target fetching a GitHub release asset (`http_fetch`), and with \
             `extract = True` one unpacking it; returns the main target's address. With \
             `credentials` (a private repository) the asset is fetched through the API.\n\n{}",
            spec_params_doc(&ReleaseAssetArgs::schema())
        ),
        func: Arc::new(ReleaseAsset {
            plugin: plugin.to_string(),
        }),
    }]
}

struct ReleaseAsset {
    plugin: String,
}

#[async_trait]
impl PluginFn for ReleaseAsset {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        release_asset(&self.plugin, ctx.pkg, &args)
            .with_context(|| format!("heph.{}.release_asset", self.plugin))
    }
}

/// The arguments of `release_asset`. The function's signature is derived from
/// this struct's schema ([`spec_params`]) and its hover doc lists the field
/// docs below, so a name, type or doc is written once. An omitted optional
/// argument arrives as `None`, which every optional field decodes as its
/// default.
///
/// The per-argument rules are the field types'; the rules between arguments
/// (what applies only with or without `extract`) are [`release_asset`]'s.
#[derive(Spec)]
struct ReleaseAssetArgs {
    /// The target name; the function returns `//<pkg>:<name>`.
    #[spec(required)]
    name: NonEmpty,
    /// The repository, `owner/name`.
    #[spec(required)]
    repo: NonEmpty,
    /// The release tag. May contain `/`.
    #[spec(required)]
    tag: NonEmpty,
    /// The asset's file name.
    #[spec(required)]
    asset: NonEmpty,
    /// The asset's SHA-256, forwarded to `http_fetch`. Without it the fetch is
    /// unverified and `http_fetch` warns.
    #[spec(ty = ParamType::String)]
    sha256: Option<String>,
    /// Unpack the asset (default `False`).
    extract: bool,
    /// With `extract` only: the archive format, `tar`, `tar.gz`, `tar.xz`,
    /// `tar.zst`, `zip`, or `gz` (one gzipped file). Inferred from `asset`'s
    /// suffix by default.
    #[spec(ty = FormatArg::spec_param_type())]
    format: Option<FormatArg>,
    /// With `extract` only: leading path components dropped, as `tar
    /// --strip-components` (default 0).
    #[spec(ty = ParamType::Int)]
    strip_components: Option<u32>,
    /// With `extract` only: the most bytes the archive may unpack to (default
    /// 4 GiB).
    #[spec(ty = ParamType::Int)]
    max_bytes: Option<NonZeroU64>,
    /// With `extract` only: the most entries the archive may hold (default
    /// 1,000,000).
    #[spec(ty = ParamType::Int)]
    max_entries: Option<NonZeroU64>,
    /// Mark the fetched file executable (default `False`); with `extract`,
    /// a `gz` asset's one unpacked file, and refused for any other format.
    executable: bool,
    /// Without `extract`: the fetched file's name (default `asset`). With it:
    /// the output directory (default `name`).
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// The GitHub server the release is on (default `https://github.com`): a
    /// GitHub Enterprise server's URL, for one.
    #[spec(ty = ParamType::String)]
    server_url: Option<String>,
    /// Its REST API, where a private asset is looked up and fetched. Default:
    /// derived from this call's `server_url`, `https://api.github.com` for
    /// github.com and `{server_url}/api/v3` for any other server.
    #[spec(ty = ParamType::String)]
    api_url: Option<String>,
    /// Credential targets for a private repository: the asset is then fetched
    /// through the API.
    #[spec(ty = ParamType::list(ParamType::String))]
    credentials: Addrs,
    /// Caching for what this declares: the asset (fetched, and with `extract`
    /// unpacked) and, with `credentials`, the release JSON. The shape every
    /// driver's `cache` takes: `False` turns off both tiers,
    /// `{"remote": False}` keeps it local-only. Default: local and remote,
    /// except the release JSON, which takes `enabled` and `history` from this
    /// but is local-only unless this is a dict saying `"remote": True`.
    #[spec(ty = TargetSpecCache::spec_param_type())]
    cache: Option<TargetSpecCache>,
}

/// A string argument that may not be empty.
struct NonEmpty(String);

impl FromSpecValue for NonEmpty {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let s = String::from_spec_value(v)?;
        if s.is_empty() {
            bail!("must be a non-empty string");
        }
        Ok(Self(s))
    }

    fn spec_param_type() -> ParamType {
        ParamType::String
    }
}

/// A list of target addresses: a list (a bare string is refused, unlike
/// `Vec<String>`), of non-empty strings. `None` is the empty list.
#[derive(Default)]
struct Addrs(Vec<String>);

impl FromSpecValue for Addrs {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        match v {
            Value::Null() => Ok(Self::default()),
            Value::List(items) => items
                .iter()
                .map(|item| match NonEmpty::from_spec_value(item) {
                    Ok(addr) => Ok(addr.0),
                    Err(e) => {
                        Err(e.context(format!("must be a list of target addresses, got {item:?}")))
                    }
                })
                .collect::<anyhow::Result<_>>()
                .map(Self),
            other => bail!("must be a list of target addresses, got {other:?}"),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::list(ParamType::String)
    }
}

/// The `format` argument: one of [`Format`]'s spellings, checked at the call
/// so a typo fails in the BUILD file rather than at the declared extract. A
/// newtype because both `Format` and [`FromSpecValue`] are other crates'.
#[derive(Clone, Copy, Debug)]
struct FormatArg(Format);

impl FromSpecValue for FormatArg {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        Format::parse(&String::from_spec_value(v)?).map(Self)
    }

    fn spec_param_type() -> ParamType {
        ParamType::one_of(Format::ALL.iter().map(|f| f.as_str()))
    }
}

/// The `cache` argument as the declared targets' own `cache` option: the dict
/// form, so each of them parses exactly what was passed.
fn cache_value(c: TargetSpecCache) -> Value {
    // Destructured, so a field added to `TargetSpecCache` fails to compile
    // here instead of being dropped on the way to the declared targets.
    let TargetSpecCache {
        local,
        remote,
        history,
    } = c;
    Value::Map(
        [
            ("enabled".to_string(), Value::Bool(local)),
            ("remote".to_string(), Value::Bool(remote)),
            ("history".to_string(), Value::Int(i64::from(history))),
        ]
        .into(),
    )
}

/// The release JSON's `cache` when the call passes none: local only (see
/// [`RELEASE_SUFFIX`]).
fn release_json_cache() -> Value {
    Value::Map([("remote".to_string(), Value::Bool(false))].into())
}

/// Whether the call's `cache` argument, as written, is a dict saying
/// `"remote": True`. [`TargetSpecCache`] cannot tell that from the default it
/// fills in for `True` or `{"history": 3}`, and only the written form sends
/// the release JSON to the remote cache.
fn writes_remote(cache: Option<&Value>) -> bool {
    matches!(cache, Some(Value::Map(m)) if matches!(m.get("remote"), Some(Value::Bool(true))))
}

/// A count argument (`max_bytes`, `max_entries`), forwarded to the extract as
/// passed: an `Int` unless it does not fit one.
fn count_value(n: NonZeroU64) -> Value {
    let n = n.get();
    i64::try_from(n).map_or(Value::Uint(n), Value::Int)
}

/// A target name the returned address can carry: what the address grammar
/// stops at (`@`, `:`, `|`, whitespace, `"`) cannot appear in it.
fn check_name(name: &str) -> anyhow::Result<()> {
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | ':' | '|' | '"' | '/'))
    {
        bail!("`name` {name:?} is not a valid target name");
    }
    Ok(())
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn headers(pairs: &[(&str, &str)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), s(v)))
            .collect(),
    )
}

/// The function's body, separated from the trait so it is a plain function of
/// its inputs.
fn release_asset(plugin: &str, pkg: &str, args: &FnArgs) -> anyhow::Result<FnOutcome> {
    let ReleaseAssetArgs {
        name: NonEmpty(name),
        repo: NonEmpty(repo),
        tag: NonEmpty(tag),
        asset: NonEmpty(asset),
        sha256,
        extract,
        format,
        strip_components,
        max_bytes,
        max_entries,
        executable,
        out,
        server_url,
        api_url,
        credentials: Addrs(credentials),
        cache,
    } = ReleaseAssetArgs::from(&args.named)?;
    // The release JSON's: the caller's `enabled` and `history`, but remote only
    // when written (see `RELEASE_SUFFIX`).
    let release_cache = cache.map_or_else(release_json_cache, |c| {
        cache_value(TargetSpecCache {
            remote: writes_remote(args.named.get("cache")),
            ..c
        })
    });
    // Forwarded only when passed, so a call without it declares for the asset
    // and the extract what it did before the argument existed.
    let cache = cache.map(cache_value);
    let cfg = &ReleaseConfig::new(
        server_url.as_deref().unwrap_or(DEFAULT_SERVER_URL),
        api_url.as_deref(),
    )?;
    let (name, repo, tag, asset) = (name.as_str(), repo.as_str(), tag.as_str(), asset.as_str());
    let out = out.as_deref();
    check_name(name)?;
    let strip = strip_components.unwrap_or(0);
    // The extract driver's limits, forwarded only when passed.
    let limits: Vec<_> = [("max_bytes", max_bytes), ("max_entries", max_entries)]
        .into_iter()
        .filter_map(|(key, count)| Some((key, count_value(count?))))
        .collect();

    let mut outcome = FnOutcome::from(Value::String(format!("//{pkg}:{name}")));

    // The asset fetch's url and auth: the public download URL, or (with
    // credentials) the API URL `<name>_url` resolves at run, fetched as bytes.
    let mut asset_fetch: HashMap<String, Value> = if credentials.is_empty() {
        [("url".to_string(), s(&asset_url(cfg, repo, tag, asset)?))].into()
    } else {
        let creds = Value::List(credentials.into_iter().map(Value::String).collect());
        let release = format!("{name}{RELEASE_SUFFIX}");
        let url_target = format!("{name}{URL_SUFFIX}");
        outcome.declare_target(DeclaredTarget {
            name: release.clone(),
            driver: HTTP_FETCH.to_string(),
            config: [
                (
                    "url".to_string(),
                    s(&release_json_url(cfg, repo, tag, asset)?),
                ),
                ("out".to_string(), s(&format!("{release}.json"))),
                (
                    "headers".to_string(),
                    headers(&[
                        ("Accept", "application/vnd.github+json"),
                        ("X-GitHub-Api-Version", GITHUB_API_VERSION),
                    ]),
                ),
                ("credentials".to_string(), creds.clone()),
                // See `RELEASE_SUFFIX`.
                ("unpinned".to_string(), Value::Bool(true)),
                ("cache".to_string(), release_cache),
            ]
            .into(),
            ..Default::default()
        });
        outcome.declare_target(DeclaredTarget {
            name: url_target.clone(),
            driver: format!("{plugin}.{}", crate::asset_url::DRIVER_NAME),
            config: [
                ("src".to_string(), s(&format!(":{release}"))),
                ("asset".to_string(), s(asset)),
                ("api".to_string(), s(&cfg.api_url)),
            ]
            .into(),
            ..Default::default()
        });
        [
            (
                "url".to_string(),
                s(&format!("${{read://{pkg}:{url_target}}}")),
            ),
            (
                "headers".to_string(),
                headers(&[("Accept", "application/octet-stream")]),
            ),
            ("credentials".to_string(), creds),
            // A 404 here means the cached release JSON is stale, not the
            // uncached `<name>_url` derived from it: name the one to force.
            (
                "refresh".to_string(),
                Value::List(vec![s(&format!(":{release}"))]),
            ),
        ]
        .into()
    };
    if let Some(sha) = sha256 {
        asset_fetch.insert("sha256".to_string(), Value::String(sha));
    }
    if let Some(cache) = &cache {
        asset_fetch.insert("cache".to_string(), cache.clone());
    }
    let fetch_config = |out: &str| {
        asset_fetch.insert("out".to_string(), s(out));
        asset_fetch
    };
    if !extract {
        if format.is_some() || strip != 0 || !limits.is_empty() {
            bail!(
                "`format`, `strip_components`, `max_bytes` and `max_entries` apply only with \
                 `extract = True`"
            );
        }
        let mut config = fetch_config(out.unwrap_or(asset));
        if executable {
            config.insert("executable".to_string(), Value::Bool(true));
        }
        outcome.declare_target(DeclaredTarget {
            name: name.to_string(),
            driver: HTTP_FETCH.to_string(),
            config,
            ..Default::default()
        });
        return Ok(outcome);
    }

    let format = match format {
        Some(FormatArg(f)) => f,
        None => Format::infer(asset).with_context(|| {
            format!(
                "cannot tell the archive format of {asset:?} from its name; set `format =` \
                 (one of: {})",
                Format::spellings().join(", ")
            )
        })?,
    };
    // A bare `gz` is one file and records no mode, so it is the one format
    // `executable` means something for; an archive's entries carry their own.
    if executable && !format.takes_executable() {
        bail!(
            "`executable` with `extract = True` applies only to a `gz` asset (one gzipped \
             file); a {} records each entry's mode",
            format.as_str()
        );
    }
    if format == Format::Gz && strip != 0 {
        bail!(
            "`strip_components` with a `gz` asset would remove its one file, which has no \
             directory to strip; drop it"
        );
    }
    if out == Some(asset) || (out.is_none() && name == asset) {
        bail!(
            "the extracted directory and the fetched archive would both be {asset:?} in the \
             package; set `out` to another directory name"
        );
    }

    let archive = format!("{name}{ARCHIVE_SUFFIX}");
    outcome.declare_target(DeclaredTarget {
        name: archive.clone(),
        driver: HTTP_FETCH.to_string(),
        config: fetch_config(asset),
        ..Default::default()
    });
    let mut config: HashMap<String, Value> = [
        ("src".to_string(), s(&format!(":{archive}"))),
        ("format".to_string(), s(format.as_str())),
    ]
    .into();
    if strip != 0 {
        config.insert("strip_components".to_string(), Value::Int(i64::from(strip)));
    }
    if executable {
        config.insert("executable".to_string(), Value::Bool(true));
    }
    config.extend(limits.into_iter().map(|(k, v)| (k.to_string(), v)));
    if let Some(out) = out {
        config.insert("out".to_string(), s(out));
    }
    if let Some(cache) = cache {
        config.insert("cache".to_string(), cache);
    }
    outcome.declare_target(DeclaredTarget {
        name: name.to_string(),
        driver: ARCHIVE_EXTRACT.to_string(),
        config,
        ..Default::default()
    });
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh() -> ReleaseConfig {
        ReleaseConfig::new(DEFAULT_SERVER_URL, None).unwrap()
    }

    fn url(repo: &str, tag: &str, asset: &str) -> anyhow::Result<String> {
        asset_url(&gh(), repo, tag, asset)
    }

    #[test]
    fn url_is_the_release_download_path() {
        assert_eq!(
            url("cli/cli", "v2.40.0", "gh_2.40.0_linux_amd64.tar.gz").unwrap(),
            "https://github.com/cli/cli/releases/download/v2.40.0/gh_2.40.0_linux_amd64.tar.gz"
        );
    }

    #[test]
    fn repo_must_be_owner_slash_name() {
        for bad in [
            "a/b/c", "/b", "a/", "a", "", "a b/c", "a/b c", "ü/x", "a/ü", "../x", "a/..", "a\\b/c",
            "a/b?x", "a/b#x",
        ] {
            let msg = format!("{:#}", url(bad, "v1", "x.tar.gz").unwrap_err());
            assert!(msg.contains("owner/name"), "{bad:?}: {msg}");
        }
        for good in ["a/b", "A-1/b.c_d", "o/.github"] {
            url(good, "v1", "x").unwrap();
        }
    }

    #[test]
    fn tag_segments_are_encoded_and_its_slashes_kept() {
        let cases = [
            ("release/v1", "release/v1"),
            ("v1#2", "v1%232"),
            ("v1%2", "v1%252"),
            ("v1?x", "v1%3Fx"),
            ("v 1", "v%201"),
            ("v{1}", "v%7B1%7D"),
            ("v1+build", "v1%2Bbuild"),
            ("ü", "%C3%BC"),
        ];
        for (tag, want) in cases {
            let got = url("o/r", tag, "a").unwrap();
            assert_eq!(
                got,
                format!("https://github.com/o/r/releases/download/{want}/a"),
                "{tag:?}"
            );
        }
        for bad in ["a//b", "/a", "a/", "..", "a/../b", "./a", "a\nb"] {
            assert!(url("o/r", bad, "a").is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn asset_is_one_encoded_segment_with_no_live_braces() {
        let got = url("o/r", "v1", "tool-{os}.tar.gz").unwrap();
        assert_eq!(
            got,
            "https://github.com/o/r/releases/download/v1/tool-%7Bos%7D.tar.gz"
        );
        for bad in ["a/b", "", ".", "..", "a\0b"] {
            assert!(url("o/r", "v1", bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn server_url_braces_are_escaped_for_the_fetch_template() {
        let cfg = ReleaseConfig::new("https://ghe.example/{x}/", None).unwrap();
        assert_eq!(
            asset_url(&cfg, "o/r", "v1", "a").unwrap(),
            "https://ghe.example/{{x}}/o/r/releases/download/v1/a"
        );
        assert_eq!(
            release_json_url(&cfg, "o/r", "v1", "a").unwrap(),
            "https://ghe.example/{{x}}/api/v3/repos/o/r/releases/tags/v1"
        );
    }

    /// In the API the tag is one path parameter, so its `/` is `%2F`; the
    /// segment checks are the download URL's, so both accept the same tags.
    #[test]
    fn the_release_lookup_encodes_the_tag_as_one_segment() {
        for (tag, want) in [
            ("v1.0", "v1.0"),
            ("release/v1", "release%2Fv1"),
            ("v1#2", "v1%232"),
            ("v{1}", "v%7B1%7D"),
            ("a/..b", "a%2F..b"),
        ] {
            assert_eq!(
                release_json_url(&gh(), "o/r", tag, "a").unwrap(),
                format!("https://api.github.com/repos/o/r/releases/tags/{want}"),
                "{tag:?}"
            );
        }
        for bad in ["a//b", "/a", "a/", "..", "a/../b", "a\nb"] {
            assert!(
                release_json_url(&gh(), "o/r", bad, "a").is_err(),
                "{bad:?} must be refused"
            );
        }
        release_json_url(&gh(), "o/r/x", "v1", "a").unwrap_err();
        release_json_url(&gh(), "o/r", "v1", "a/b").unwrap_err();
    }

    #[test]
    fn the_api_url_is_derived_from_the_server_or_set() {
        let api =
            |server: &str, api: Option<&str>| ReleaseConfig::new(server, api).map(|c| c.api_url);
        assert_eq!(api("https://github.com", None).unwrap(), GITHUB_API_URL);
        assert_eq!(api("https://github.com/", None).unwrap(), GITHUB_API_URL);
        assert_eq!(api("https://GitHub.com", None).unwrap(), GITHUB_API_URL);
        assert_eq!(
            api("https://ghe.example", None).unwrap(),
            "https://ghe.example/api/v3"
        );
        assert_eq!(
            api("https://ghe.example/sub/", None).unwrap(),
            "https://ghe.example/sub/api/v3"
        );
        assert_eq!(
            api("https://github.com", Some("http://127.0.0.1:9/")).unwrap(),
            "http://127.0.0.1:9"
        );
        for bad in [
            "api.github.com",
            "ftp://x",
            "https://",
            "https://x?y",
            "https://a b",
        ] {
            let msg = format!("{:#}", api(DEFAULT_SERVER_URL, Some(bad)).unwrap_err());
            assert!(msg.contains("`api_url`"), "{bad:?}: {msg}");
        }
    }

    #[test]
    fn server_url_is_validated() {
        for bad in [
            "github.com",
            "ftp://x",
            "https://",
            "https://a b",
            "https://x?y",
        ] {
            let msg = format!("{:#}", ReleaseConfig::new(bad, None).unwrap_err());
            assert!(msg.contains("`server_url`"), "{bad:?}: {msg}");
        }
        let cfg = ReleaseConfig::new("http://127.0.0.1:8080/", None).unwrap();
        assert_eq!(cfg.server_url, "http://127.0.0.1:8080");
    }

    /// A URL with no host, or a `user:password@` (a secret that would be
    /// written into every declared target's URL and every log line), is
    /// refused in both arguments, and the refusal does not echo the secret.
    #[test]
    fn base_urls_need_a_host_and_carry_no_userinfo() {
        let check = |server: &str, api: Option<&str>| {
            ReleaseConfig::new(server, api).map_err(|e| format!("{e:#}"))
        };
        for bad in ["https:///x", "https://:443/x", "http:///", "https://:8080"] {
            let msg = check(bad, None).unwrap_err();
            assert!(msg.contains("`server_url`"), "{bad:?}: {msg}");
            let msg = check(DEFAULT_SERVER_URL, Some(bad)).unwrap_err();
            assert!(msg.contains("`api_url`"), "{bad:?}: {msg}");
        }
        for bad in [
            "https://user:hunter2@ghe.example",
            "https://hunter2@ghe.example/sub",
            "http://user:hunter2@127.0.0.1:9",
            "ftp://user:hunter2@x",
        ] {
            for (msg, arg) in [
                (check(bad, None).unwrap_err(), "`server_url`"),
                (
                    check(DEFAULT_SERVER_URL, Some(bad)).unwrap_err(),
                    "`api_url`",
                ),
            ] {
                assert!(msg.contains(arg), "{bad:?}: {msg}");
                assert!(msg.contains("user or password"), "{bad:?}: {msg}");
                assert!(!msg.contains("hunter2"), "the secret is echoed: {msg}");
            }
        }
        // An `@` past the host is a path, not userinfo.
        check("https://ghe.example/@team", None).unwrap();
        check("http://[::1]:8080", None).unwrap();
    }

    /// `http://github.com` is not treated as github.com: its API would be
    /// derived as `http://github.com/api/v3`, which does not exist, so it is
    /// refused with the spelling that works. Another host over http is a
    /// GitHub Enterprise (or a test server) and is kept.
    #[test]
    fn http_github_com_is_refused() {
        for bad in [
            "http://github.com",
            "http://GitHub.com/",
            "http://github.com:80",
            "http://github.com/sub",
        ] {
            let msg = format!("{:#}", ReleaseConfig::new(bad, None).unwrap_err());
            assert!(msg.contains("write https://github.com"), "{bad:?}: {msg}");
        }
        for good in [
            "http://github.company.example",
            "http://ghe.example/github.com",
        ] {
            let cfg = ReleaseConfig::new(good, None).unwrap();
            assert_eq!(cfg.api_url, format!("{good}/api/v3"));
        }
    }

    /// An explicit `api_url` with a GitHub Enterprise `server_url`: downloads
    /// come from the server, lookups from the API, neither derived from the
    /// other.
    #[test]
    fn an_explicit_api_url_with_an_enterprise_server() {
        let cfg =
            ReleaseConfig::new("https://ghe.example/", Some("https://api.ghe.example/")).unwrap();
        assert_eq!(
            asset_url(&cfg, "o/r", "v1", "a").unwrap(),
            "https://ghe.example/o/r/releases/download/v1/a"
        );
        assert_eq!(
            release_json_url(&cfg, "o/r", "v1", "a").unwrap(),
            "https://api.ghe.example/repos/o/r/releases/tags/v1"
        );
    }

    fn call(pairs: &[(&str, Value)]) -> anyhow::Result<FnOutcome> {
        let args = FnArgs {
            positional: vec![],
            named: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        };
        release_asset("gh", "third_party/tool", &args)
    }

    fn base() -> Vec<(&'static str, Value)> {
        vec![
            ("name", s("tool")),
            ("repo", s("o/r")),
            ("tag", s("v1")),
            ("asset", s("tool-linux.tar.gz")),
        ]
    }

    fn with(extra: &[(&'static str, Value)]) -> Vec<(&'static str, Value)> {
        let mut v = base();
        v.extend(extra.iter().cloned());
        v
    }

    const URL: &str = "https://github.com/o/r/releases/download/v1/tool-linux.tar.gz";

    #[test]
    fn without_extract_it_declares_one_fetch() {
        let out = call(&with(&[
            ("sha256", s("abc")),
            ("executable", Value::Bool(true)),
        ]))
        .unwrap();
        assert_eq!(out.value(), &s("//third_party/tool:tool"));
        let [t] = out.targets() else {
            panic!("one target: {:?}", out.targets())
        };
        assert_eq!(t.name, "tool");
        assert_eq!(t.driver, "http_fetch");
        assert_eq!(t.config.get("url"), Some(&s(URL)));
        assert_eq!(t.config.get("out"), Some(&s("tool-linux.tar.gz")));
        assert_eq!(t.config.get("sha256"), Some(&s("abc")));
        assert_eq!(t.config.get("executable"), Some(&Value::Bool(true)));
        assert!(out.states().is_empty());

        // Nothing the user did not pass is forwarded.
        let out = call(&with(&[("out", s("bin"))])).unwrap();
        let [t] = out.targets() else { panic!() };
        assert_eq!(t.config.len(), 2, "{:?}", t.config);
        assert_eq!(t.config.get("out"), Some(&s("bin")));
    }

    #[test]
    fn with_extract_it_declares_a_fetch_and_an_extract() {
        let out = call(&with(&[
            ("extract", Value::Bool(true)),
            ("strip_components", Value::Int(1)),
            ("sha256", s("abc")),
        ]))
        .unwrap();
        assert_eq!(out.value(), &s("//third_party/tool:tool"));
        let [fetch, extract] = out.targets() else {
            panic!("two targets: {:?}", out.targets())
        };
        assert_eq!(fetch.name, "tool_archive");
        assert_eq!(fetch.driver, "http_fetch");
        assert_eq!(fetch.config.get("url"), Some(&s(URL)));
        assert_eq!(fetch.config.get("out"), Some(&s("tool-linux.tar.gz")));
        assert_eq!(fetch.config.get("sha256"), Some(&s("abc")));

        assert_eq!(extract.name, "tool");
        assert_eq!(extract.driver, "archive.extract");
        assert_eq!(extract.config.get("src"), Some(&s(":tool_archive")));
        assert_eq!(extract.config.get("format"), Some(&s("tar.gz")));
        assert_eq!(extract.config.get("strip_components"), Some(&Value::Int(1)));
        assert_eq!(extract.config.get("out"), None, "defaults in the driver");
        assert_eq!(
            extract.config.get("max_bytes"),
            None,
            "defaults in the driver"
        );
        assert_eq!(
            extract.config.get("max_entries"),
            None,
            "defaults in the driver"
        );
    }

    /// A `.tar.xz` and a bare `.gz` asset infer their format from the name;
    /// only the `gz`, one file with no recorded mode, takes `executable`, and
    /// it reaches the extract, not the fetch.
    #[test]
    fn xz_and_gz_assets_infer_their_format_and_gz_takes_executable() {
        let asset = |name: &'static str, extra: &[(&'static str, Value)]| {
            let mut args: Vec<_> = base().into_iter().filter(|(k, _)| *k != "asset").collect();
            args.extend([("asset", s(name)), ("extract", Value::Bool(true))]);
            args.extend_from_slice(extra);
            call(&args)
        };
        let out = asset("tool-linux.tar.xz", &[]).unwrap();
        assert_eq!(
            target(&out, "tool").config.get("format"),
            Some(&s("tar.xz"))
        );

        let exe = [("executable", Value::Bool(true))];
        let out = asset("tool-linux.gz", &exe).unwrap();
        let extract = target(&out, "tool");
        assert_eq!(extract.config.get("format"), Some(&s("gz")));
        assert_eq!(extract.config.get("executable"), Some(&Value::Bool(true)));
        let fetch = target(&out, "tool_archive");
        assert_eq!(fetch.config.get("out"), Some(&s("tool-linux.gz")));
        assert!(
            !fetch.config.contains_key("executable"),
            "the archive is data"
        );

        let plain = asset("tool-linux.gz", &[]).unwrap();
        assert!(!target(&plain, "tool").config.contains_key("executable"));

        let msg = format!("{:#}", asset("tool-linux.tar.xz", &exe).unwrap_err());
        assert!(msg.contains("a tar.xz records each entry's mode"), "{msg}");

        // A `gz` is one file: `strip_components` could only fail at run, so
        // it is refused here, whether the format is inferred or named.
        let strip = ("strip_components", Value::Int(1));
        for extra in [
            vec![strip.clone()],
            vec![strip.clone(), ("format", s("gz"))],
        ] {
            let msg = format!("{:#}", asset("tool-linux.gz", &extra).unwrap_err());
            assert!(
                msg.contains("`strip_components` with a `gz` asset"),
                "{msg}"
            );
        }
        let out = asset("tool-linux.tar.xz", &[strip]).unwrap();
        assert_eq!(
            target(&out, "tool").config.get("strip_components"),
            Some(&Value::Int(1))
        );
    }

    /// The unpack limits reach the extract target as passed, and only there.
    #[test]
    fn the_unpack_limits_are_forwarded_to_the_extract() {
        let out = call(&with(&[
            ("extract", Value::Bool(true)),
            ("max_bytes", Value::Int(1 << 33)),
            ("max_entries", Value::Uint(5)),
        ]))
        .unwrap();
        let [fetch, extract] = out.targets() else {
            panic!("two targets: {:?}", out.targets())
        };
        assert_eq!(extract.config.get("max_bytes"), Some(&Value::Int(1 << 33)));
        // As an `Int`, whatever the caller's spelling: the extract reads a count.
        assert_eq!(extract.config.get("max_entries"), Some(&Value::Int(5)));
        assert!(
            !fetch.config.contains_key("max_bytes") && !fetch.config.contains_key("max_entries"),
            "{:?}",
            fetch.config
        );

        let out = call(&with(&[
            ("extract", Value::Bool(true)),
            ("max_entries", Value::Int(9)),
        ]))
        .unwrap();
        let extract = target(&out, "tool");
        assert_eq!(extract.config.get("max_entries"), Some(&Value::Int(9)));
        assert_eq!(extract.config.get("max_bytes"), None);
    }

    /// The plugin's own driver (`asset_url`) is named after the plugin as the
    /// manifest names it; the extract is the builtin `archive.extract` under
    /// any name.
    #[test]
    fn only_the_plugins_own_driver_is_named_after_it() {
        let args = FnArgs {
            positional: vec![],
            named: with(&[("extract", Value::Bool(true)), creds(&["//auth:gh"])])
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        };
        let out = release_asset("github", "", &args).unwrap();
        assert_eq!(out.value(), &s("//:tool"));
        assert_eq!(target(&out, "tool_url").driver, "github.asset_url");
        assert_eq!(target(&out, "tool").driver, "archive.extract");
    }

    fn creds(addrs: &[&str]) -> (&'static str, Value) {
        (
            "credentials",
            Value::List(addrs.iter().map(|a| s(a)).collect()),
        )
    }

    fn target<'a>(out: &'a FnOutcome, name: &str) -> &'a DeclaredTarget {
        out.targets()
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no target {name}: {:?}", out.targets()))
    }

    /// With credentials: the release lookup (uncached, API headers), the
    /// `asset_url` over it, and the asset fetched from the URL it resolves to,
    /// as bytes. The credentials go to both fetches and nowhere else.
    #[test]
    fn with_credentials_it_fetches_through_the_api() {
        let out = call(&with(&[
            creds(&["//auth:gh"]),
            ("sha256", s("abc")),
            ("executable", Value::Bool(true)),
        ]))
        .unwrap();
        assert_eq!(out.value(), &s("//third_party/tool:tool"));
        assert_eq!(out.targets().len(), 3, "{:?}", out.targets());
        let gh_creds = Value::List(vec![s("//auth:gh")]);

        let release = target(&out, "tool_release");
        assert_eq!(release.driver, "http_fetch");
        assert_eq!(
            release.config.get("url"),
            Some(&s("https://api.github.com/repos/o/r/releases/tags/v1"))
        );
        assert_eq!(release.config.get("out"), Some(&s("tool_release.json")));
        assert_eq!(
            release.config.get("headers"),
            Some(&headers(&[
                ("Accept", "application/vnd.github+json"),
                ("X-GitHub-Api-Version", "2022-11-28"),
            ]))
        );
        assert_eq!(release.config.get("credentials"), Some(&gh_creds));
        assert_eq!(
            release.config.get("cache"),
            Some(&Value::Map(
                [("remote".to_string(), Value::Bool(false))].into()
            )),
            "the JSON is cached, locally only"
        );
        assert_eq!(release.config.get("sha256"), None, "the JSON is not pinned");
        assert_eq!(
            release.config.get("unpinned"),
            Some(&Value::Bool(true)),
            "and says so, so http_fetch does not warn about it"
        );
        assert_eq!(release.config.len(), 6, "{:?}", release.config);

        let url = target(&out, "tool_url");
        assert_eq!(url.driver, "gh.asset_url");
        assert_eq!(url.config.get("src"), Some(&s(":tool_release")));
        assert_eq!(url.config.get("asset"), Some(&s("tool-linux.tar.gz")));
        assert_eq!(url.config.get("api"), Some(&s("https://api.github.com")));
        assert_eq!(url.config.len(), 3, "no credentials: it reads a file");

        let fetch = target(&out, "tool");
        assert_eq!(fetch.driver, "http_fetch");
        assert_eq!(
            fetch.config.get("url"),
            Some(&s("${read://third_party/tool:tool_url}"))
        );
        assert_eq!(
            fetch.config.get("headers"),
            Some(&headers(&[("Accept", "application/octet-stream")]))
        );
        assert_eq!(fetch.config.get("credentials"), Some(&gh_creds));
        assert_eq!(fetch.config.get("sha256"), Some(&s("abc")));
        assert_eq!(fetch.config.get("out"), Some(&s("tool-linux.tar.gz")));
        assert_eq!(fetch.config.get("executable"), Some(&Value::Bool(true)));
        assert_eq!(fetch.config.get("cache"), None, "the asset is cached");
        assert_eq!(
            fetch.config.get("refresh"),
            Some(&Value::List(vec![s(":tool_release")])),
            "a 404 names the cached JSON, not the uncached url"
        );
    }

    /// `cache` reaches every target that stores something — the release JSON,
    /// the fetched archive and the unpacked tree — and not `<name>_url`, which
    /// is never cached. The asset and the extract take it as passed, and carry
    /// no `cache` key without it. The release JSON takes its `enabled` and
    /// `history`, but goes to the remote cache only when the call writes
    /// `"remote": True`: `True` or `{"history": 3}` leave it local-only.
    #[test]
    fn cache_is_forwarded_to_every_stored_target() {
        let tsc = |local, remote, history| TargetSpecCache {
            local,
            remote,
            history,
        };
        let map = |pairs: &[(&str, Value)]| {
            Value::Map(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect(),
            )
        };
        // (cache passed, release JSON's, asset's and extract's, why)
        let rows = [
            (None, tsc(true, false, 1), None, "default: local-only"),
            (
                Some(Value::Bool(true)),
                tsc(true, false, 1),
                Some(tsc(true, true, 1)),
                "True: local-only",
            ),
            (
                Some(map(&[("history", Value::Int(3))])),
                tsc(true, false, 3),
                Some(tsc(true, true, 3)),
                "history: local-only, history 3",
            ),
            (
                Some(map(&[("remote", Value::Bool(true))])),
                tsc(true, true, 1),
                Some(tsc(true, true, 1)),
                "remote written: remote",
            ),
            (
                Some(Value::Bool(false)),
                tsc(false, false, 1),
                Some(tsc(false, false, 1)),
                "False: off",
            ),
        ];
        for (passed, release, rest, why) in rows {
            let mut args = with(&[creds(&["//auth:gh"]), ("extract", Value::Bool(true))]);
            args.extend(passed.map(|v| ("cache", v)));
            let out = call(&args).unwrap();
            let cache_of = |name: &str| {
                target(&out, name)
                    .config
                    .get("cache")
                    .map(|v| TargetSpecCache::from_spec_value(v).unwrap())
            };
            assert_eq!(cache_of("tool_release"), Some(release), "{why}");
            for name in ["tool_archive", "tool"] {
                assert_eq!(cache_of(name), rest, "{why}: {name}");
            }
            assert_eq!(cache_of("tool_url"), None, "{why}");
        }

        let err = call(&with(&[("cache", s("remote"))])).unwrap_err();
        assert!(format!("{err:#}").contains("cache"), "{err:#}");
    }

    #[test]
    fn with_credentials_and_extract_the_archive_is_the_api_fetch() {
        let out = call(&with(&[
            creds(&["//auth:gh", "//auth:other"]),
            ("extract", Value::Bool(true)),
            ("sha256", s("abc")),
        ]))
        .unwrap();
        assert_eq!(out.targets().len(), 4, "{:?}", out.targets());
        let both = Value::List(vec![s("//auth:gh"), s("//auth:other")]);
        let release = target(&out, "tool_release");
        assert_eq!(release.config.get("credentials"), Some(&both));
        assert_eq!(release.config.get("sha256"), None, "the JSON is not pinned");
        assert_eq!(target(&out, "tool_url").config.get("sha256"), None);
        let archive = target(&out, "tool_archive");
        assert_eq!(
            archive.config.get("url"),
            Some(&s("${read://third_party/tool:tool_url}"))
        );
        assert_eq!(archive.config.get("credentials"), Some(&both));
        assert_eq!(archive.config.get("out"), Some(&s("tool-linux.tar.gz")));
        assert_eq!(
            archive.config.get("sha256"),
            Some(&s("abc")),
            "the pin is the asset's bytes"
        );
        let extract = target(&out, "tool");
        assert_eq!(extract.driver, "archive.extract");
        assert_eq!(extract.config.get("src"), Some(&s(":tool_archive")));
        assert_eq!(extract.config.get("credentials"), None);
        assert_eq!(extract.config.get("sha256"), None);
    }

    /// GHES serves its API under the server, unless the call's `api_url` says
    /// otherwise; an empty `credentials` list is the public path; and a
    /// non-string entry is refused.
    #[test]
    fn credentials_follow_the_api_url_and_are_checked() {
        let ghe = |extra: &[(&'static str, Value)]| {
            let mut args = with(&[
                creds(&["//auth:ghe"]),
                ("tag", s("release/v1")),
                ("server_url", s("https://ghe.example")),
            ]);
            args.extend(extra.iter().cloned());
            let named = args.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            release_asset(
                "gh",
                "",
                &FnArgs {
                    positional: vec![],
                    named,
                },
            )
            .unwrap()
        };
        let out = ghe(&[]);
        assert_eq!(
            target(&out, "tool_release").config.get("url"),
            Some(&s(
                "https://ghe.example/api/v3/repos/o/r/releases/tags/release%2Fv1"
            ))
        );
        assert_eq!(
            target(&out, "tool_url").config.get("api"),
            Some(&s("https://ghe.example/api/v3"))
        );
        assert_eq!(
            target(&out, "tool").config.get("url"),
            Some(&s("${read://:tool_url}"))
        );

        let out = ghe(&[("api_url", s("https://api.ghe.example/"))]);
        assert_eq!(
            target(&out, "tool_release").config.get("url"),
            Some(&s(
                "https://api.ghe.example/repos/o/r/releases/tags/release%2Fv1"
            ))
        );
        assert_eq!(
            target(&out, "tool_url").config.get("api"),
            Some(&s("https://api.ghe.example"))
        );

        let public = call(&with(&[creds(&[])])).unwrap();
        let [t] = public.targets() else { panic!() };
        assert_eq!(t.config.get("url"), Some(&s(URL)));

        for bad in [
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![s("")]),
            s("//auth:gh"),
        ] {
            let msg = format!(
                "{:#}",
                call(&with(&[("credentials", bad.clone())])).unwrap_err()
            );
            assert!(msg.contains("`credentials`"), "{bad:?}: {msg}");
        }
    }

    #[test]
    fn an_explicit_format_wins_and_an_unknown_suffix_needs_one() {
        let out = call(&[
            ("name", s("tool")),
            ("repo", s("o/r")),
            ("tag", s("v1")),
            ("asset", s("tool.bin")),
            ("extract", Value::Bool(true)),
            ("format", s("zip")),
        ])
        .unwrap();
        assert_eq!(out.targets()[1].config.get("format"), Some(&s("zip")));

        let err = call(&[
            ("name", s("tool")),
            ("repo", s("o/r")),
            ("tag", s("v1")),
            ("asset", s("tool.tar.gz.sha256")),
            ("extract", Value::Bool(true)),
        ])
        .unwrap_err();
        assert!(format!("{err:#}").contains("format ="), "{err:#}");
    }

    #[test]
    fn misused_options_are_refused() {
        let cases: [(&[(&'static str, Value)], &str); 10] = [
            (
                &[("max_bytes", Value::Int(1))],
                "only with `extract = True`",
            ),
            (
                &[("max_entries", Value::Int(1))],
                "only with `extract = True`",
            ),
            (
                &[("extract", Value::Bool(true)), ("max_bytes", Value::Int(0))],
                "parse `max_bytes`: invalid: expected a positive integer, got: 0",
            ),
            (
                &[
                    ("extract", Value::Bool(true)),
                    ("max_entries", Value::Int(-3)),
                ],
                "parse `max_entries`: invalid: expected a positive integer, got: -3",
            ),
            (
                &[("extract", Value::Bool(true)), ("max_bytes", s("4G"))],
                "parse `max_bytes`: invalid: expected int",
            ),
            (
                &[
                    ("extract", Value::Bool(true)),
                    ("executable", Value::Bool(true)),
                ],
                "applies only to a `gz` asset",
            ),
            (&[("format", s("tar"))], "only with `extract = True`"),
            (
                &[("strip_components", Value::Int(1))],
                "only with `extract = True`",
            ),
            (
                &[
                    ("extract", Value::Bool(true)),
                    ("strip_components", Value::Int(-1)),
                ],
                "parse `strip_components`: invalid: expected a non-negative integer",
            ),
            (
                &[
                    ("extract", Value::Bool(true)),
                    ("out", s("tool-linux.tar.gz")),
                ],
                "set `out`",
            ),
        ];
        for (extra, want) in cases {
            let msg = format!("{:#}", call(&with(extra)).unwrap_err());
            assert!(msg.contains(want), "{extra:?}: {msg}");
        }
        for bad_name in ["a:b", "a b", "a@x", "", "a/b"] {
            let mut args = base();
            args[0] = ("name", s(bad_name));
            assert!(call(&args).is_err(), "{bad_name:?}");
        }
        // Each required argument: missing, empty, and the wrong type, each
        // naming the argument.
        for i in 0..base().len() {
            let key = base()[i].0;
            let mut missing = base();
            missing.remove(i);
            let msg = format!("{:#}", call(&missing).unwrap_err());
            assert_eq!(msg, format!("missing required `{key}`"));
            for (bad, want) in [
                (s(""), "must be a non-empty string"),
                (Value::Int(1), "expected string"),
            ] {
                let mut args = base();
                args[i].1 = bad;
                let msg = format!("{:#}", call(&args).unwrap_err());
                assert!(msg.starts_with(&format!("parse `{key}`: ")), "{msg}");
                assert!(msg.contains(want), "{msg}");
            }
        }
    }

    /// The server is the call's: a GitHub Enterprise `server_url` moves the
    /// download URL of that call only, and a bad one (or a bad `api_url`)
    /// fails the call through the signature, naming the argument.
    #[tokio::test]
    async fn the_server_is_a_per_call_argument() {
        let ghe = call(&with(&[("server_url", s("https://ghe.example/"))])).unwrap();
        let [t] = ghe.targets() else { panic!() };
        assert_eq!(
            t.config.get("url"),
            Some(&s(
                "https://ghe.example/o/r/releases/download/v1/tool-linux.tar.gz"
            ))
        );
        let public = call(&base()).unwrap();
        let [t] = public.targets() else { panic!() };
        assert_eq!(
            t.config.get("url"),
            Some(&s(URL)),
            "the default is github.com"
        );

        for bad in ["github.com", "https://", "https://x?y"] {
            let msg = format!(
                "{:#}",
                call_through_signature(&with(&[("server_url", s(bad))]))
                    .await
                    .unwrap_err()
            );
            for want in ["heph.gh.release_asset", "`server_url`"] {
                assert!(msg.contains(want), "{bad:?}: {want}: {msg}");
            }
        }
        let msg = format!(
            "{:#}",
            call_through_signature(&with(&[("api_url", s("api.ghe.example"))]))
                .await
                .unwrap_err()
        );
        for want in ["heph.gh.release_asset", "`api_url`"] {
            assert!(msg.contains(want), "{want}: {msg}");
        }
    }

    #[test]
    fn the_signature_lists_every_argument() {
        let defs = functions("gh");
        let [def] = defs.as_slice() else { panic!() };
        assert_eq!(def.name, "release_asset");
        let names: Vec<_> = def.signature.named.iter().map(|p| p.name).collect();
        for want in [
            "api_url",
            "asset",
            "credentials",
            "executable",
            "extract",
            "format",
            "max_bytes",
            "max_entries",
            "name",
            "out",
            "repo",
            "server_url",
            "sha256",
            "strip_components",
            "tag",
        ] {
            assert!(names.contains(&want), "{want} missing: {names:?}");
        }
        // Exactly the decoding struct's fields, so an argument the signature
        // accepts is one the call decodes, and the other way round.
        let mut keys = <ReleaseAssetArgs as hplugin::htspec::SpecFields>::KEYS.to_vec();
        keys.sort_unstable();
        assert_eq!(names, keys);
        let required: Vec<_> = def
            .signature
            .named
            .iter()
            .filter(|p| p.default.is_none())
            .map(|p| p.name)
            .collect();
        assert_eq!(required, ["asset", "name", "repo", "tag"]);
    }

    /// Calls the function as the Starlark bridge does: the signature first
    /// (which fills each omitted optional argument with its `None` default),
    /// then the handler.
    async fn call_through_signature(pairs: &[(&str, Value)]) -> anyhow::Result<FnOutcome> {
        let defs = functions("gh");
        let [def] = defs.as_slice() else { panic!() };
        let (positional, named) = def.signature.validate_args(
            "heph.gh.release_asset",
            vec![],
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )?;
        let ctx = FnCallContext {
            pkg: "third_party/tool",
            root: std::path::Path::new("/"),
        };
        def.func.call(&ctx, FnArgs { positional, named }).await
    }

    /// With only the required arguments, every default the signature fills in
    /// decodes as the argument's documented default: one plain fetch of the
    /// asset, under its own name, not executable, unverified.
    #[tokio::test]
    async fn the_signature_defaults_decode_as_the_documented_defaults() {
        let via_sig = call_through_signature(&base()).await.unwrap();
        let direct = call(&base()).unwrap();
        assert_eq!(via_sig.targets(), direct.targets());
        let [fetch] = via_sig.targets() else {
            panic!("one target: {:?}", via_sig.targets())
        };
        assert_eq!(fetch.config.get("url"), Some(&s(URL)));
        assert_eq!(fetch.config.get("out"), Some(&s("tool-linux.tar.gz")));
        assert!(
            !fetch.config.contains_key("sha256") && !fetch.config.contains_key("executable"),
            "{:?}",
            fetch.config
        );

        // And with `extract`, the extract's defaults: no limits forwarded.
        let out = call_through_signature(&with(&[("extract", Value::Bool(true))]))
            .await
            .unwrap();
        let [_, extract] = out.targets() else {
            panic!("two targets: {:?}", out.targets())
        };
        assert!(
            !extract.config.contains_key("max_bytes")
                && !extract.config.contains_key("max_entries"),
            "{:?}",
            extract.config
        );
    }

    /// A deferred value cannot pick the asset: it decides the declared
    /// targets' URL and name, which the graph needs before anything runs.
    #[tokio::test]
    async fn a_deferred_reference_in_asset_is_refused() {
        let mut args = base();
        args[3] = ("asset", s("${read://p:x}"));
        let msg = format!("{:#}", call_through_signature(&args).await.unwrap_err());
        assert!(msg.contains("parse `asset`"), "{msg}");
        assert!(msg.contains("deferred value reference"), "{msg}");
    }
}
