//! The `asset_url` driver (`<plugin>.asset_url`, `gh.asset_url` under the
//! shipped manifest): pick one asset's API URL out of a GitHub release JSON.
//!
//! A private repository's asset is not served at its `releases/download/…`
//! URL to a token; it is fetched from the API by id. `release_asset` with
//! `credentials` declares an `http_fetch` of `GET /repos/{repo}/releases/tags/{tag}`,
//! this driver over its output, and an `http_fetch` whose `url` is
//! `${read://…}` of this one's. So all network stays in `http_fetch`, and this
//! is a pure function of the JSON's bytes, `asset` and `api`. See
//! `docs/GH_RELEASES.md`.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunRequest, ManagedRunResponse, single_dep_file,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// The driver's local name; the engine reaches it as `<plugin>.asset_url`.
pub const DRIVER_NAME: &str = "asset_url";

/// Bump to invalidate every resolved URL when what this driver writes changes.
const ASSET_URL_FORMAT_VERSION: u32 = 1;

/// The dep input's origin id.
const SRC_ORIGIN: &str = "asset_url:src";

/// How many asset names a "not found" error lists before counting the rest.
const LISTED_NAMES: usize = 20;

/// Config for an `asset_url` target.
#[derive(Spec)]
struct AssetUrlSpec {
    /// Address of the target whose single output file is the release JSON
    /// (`GET /repos/{owner}/{repo}/releases/tags/{tag}`).
    #[spec(required)]
    src: String,
    /// The asset's file name, as the release lists it.
    #[spec(required)]
    asset: String,
    /// The API base URL the asset's URL must be under, no trailing `/`
    /// (`https://api.github.com`, or `https://<host>/api/v3`).
    #[spec(required)]
    api: String,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AssetUrlDef {
    asset: String,
    api: String,
}

/// The part of a release JSON this driver reads; every other field is ignored.
#[derive(serde::Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    /// The asset's API URL, `{api}/repos/{owner}/{repo}/releases/assets/{id}`.
    url: String,
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
        AssetUrlSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = AssetUrlSpec::from(&req.target_spec.config).context("parse asset_url config")?;
        anyhow::ensure!(
            !spec.asset.is_empty(),
            "{}: `asset` must not be empty",
            addr.format()
        );
        let pkg = addr.package.clone();
        let src = TargetAddr::parse(&spec.src, &pkg)
            .with_context(|| format!("parsing asset_url src {:?}", spec.src))?;
        let out = if pkg.as_str().is_empty() {
            addr.name.clone()
        } else {
            format!("{}/{}", pkg.as_str(), addr.name)
        };

        let mut h = Xxh3::new();
        h.update(&ASSET_URL_FORMAT_VERSION.to_le_bytes());
        h.update(addr.format().as_bytes());
        h.update(b"\0");
        // The JSON's bytes reach the key through the dep's hashout, so only
        // the address goes in here — all of it, `|group` and `[filters]`
        // included, since they pick which of the dep's files is read.
        h.update(src.to_string().as_bytes());
        h.update(b"\0");
        h.update(spec.asset.as_bytes());
        h.update(b"\0");
        h.update(spec.api.as_bytes());
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(AssetUrlDef {
                    asset: spec.asset,
                    api: spec.api,
                }),
                inputs: vec![Input {
                    r#ref: src,
                    mode: InputMode::Standard,
                    origin_id: SRC_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                }],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                hash,
                // Not in `hash` above: a declaration moves no key.
                resources: spec.resources,
                // Uncached (`CacheConfig::off()`, the default): its input, the
                // release JSON, is itself uncached and re-read every build, so
                // a cache entry would be keyed on fresh bytes each time and
                // never hit, while the run is a JSON parse. The fetch below it
                // still hits while the URL it writes is unchanged.
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
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<AssetUrlDef>().clone();
        let addr = req.request.target.addr.format();
        let context = format!("{addr}: release asset {:?}", def.asset);
        let src =
            single_dep_file(&req.inputs, SRC_ORIGIN).with_context(|| format!("{addr}: `src`"))?;
        // The output is `<pkg>/<name>` (see `parse`).
        let dest = req.sandbox_pkg_dir.join(&req.request.target.addr.name);
        hcore::blocking::run(move || {
            let json = src
                .single_file()
                .with_context(|| format!("{addr}: `src`"))?;
            let bytes = std::fs::read(&json).with_context(|| format!("read {}", json.display()))?;
            let url = pick(&bytes, &def.asset, &def.api)?;
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
            std::fs::write(&dest, format!("{url}\n"))
                .with_context(|| format!("write {}", dest.display()))
        })
        .await
        .context(context)?;
        Ok(ManagedRunResponse::default())
    }
}

/// The API URL of the one asset named `asset` in the release JSON `bytes`.
///
/// The URL must be under `api` (`{api}/…`): it is what a credentialed fetch is
/// sent to next, so a JSON naming another host or path is refused rather than
/// followed. It must also be one line, since it is written to a file the next
/// fetch reads whole.
fn pick(bytes: &[u8], asset: &str, api: &str) -> anyhow::Result<String> {
    let release: Release = serde_json::from_slice(bytes).context(
        "the release lookup did not return a GitHub release (a JSON object with `assets`)",
    )?;
    let mut matching = release.assets.iter().filter(|a| a.name == asset);
    let Some(found) = matching.next() else {
        let names: Vec<&str> = release
            .assets
            .iter()
            .take(LISTED_NAMES)
            .map(|a| a.name.as_str())
            .collect();
        let more = release.assets.len().saturating_sub(LISTED_NAMES);
        anyhow::bail!(
            "the release has no asset named {asset:?}; it has {}{}",
            if names.is_empty() {
                "none".to_string()
            } else {
                format!("{names:?}")
            },
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        );
    };
    anyhow::ensure!(
        matching.next().is_none(),
        "the release lists more than one asset named {asset:?}"
    );
    let url = found.url.as_str();
    anyhow::ensure!(
        !url.chars().any(|c| c.is_whitespace() || c.is_control()),
        "the release gives asset {asset:?} the url {url:?}, which contains whitespace or a \
         control character"
    );
    anyhow::ensure!(
        url.strip_prefix(api)
            .is_some_and(|rest| rest.starts_with('/')),
        "the release gives asset {asset:?} the url {url:?}, which is not under the API \
         {api:?}; credentials are only sent there"
    );
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    const API: &str = "https://api.github.com";

    fn release(assets: &[(&str, &str)]) -> Vec<u8> {
        let assets: Vec<String> = assets
            .iter()
            .map(|(n, u)| format!(r#"{{"name":{n:?},"url":{u:?},"download_count":3}}"#))
            .collect();
        format!(r#"{{"tag_name":"v1","assets":[{}]}}"#, assets.join(",")).into_bytes()
    }

    #[test]
    fn picks_the_named_assets_api_url() {
        let json = release(&[
            (
                "a.tar.gz",
                "https://api.github.com/repos/o/r/releases/assets/1",
            ),
            (
                "b.zip",
                "https://api.github.com/repos/o/r/releases/assets/2",
            ),
        ]);
        assert_eq!(
            pick(&json, "b.zip", API).unwrap(),
            "https://api.github.com/repos/o/r/releases/assets/2"
        );
    }

    #[test]
    fn a_missing_asset_lists_what_the_release_has() {
        let json = release(&[("a.tar.gz", "https://api.github.com/x/1")]);
        let msg = format!("{:#}", pick(&json, "b.zip", API).unwrap_err());
        assert!(msg.contains(r#"no asset named "b.zip""#), "{msg}");
        assert!(msg.contains(r#"["a.tar.gz"]"#), "{msg}");

        let msg = format!("{:#}", pick(&release(&[]), "b.zip", API).unwrap_err());
        assert!(msg.contains("it has none"), "{msg}");

        let many: Vec<(String, String)> = (0..25)
            .map(|i| (format!("f{i}"), format!("{API}/x/{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(n, u)| (n.as_str(), u.as_str())).collect();
        let msg = format!("{:#}", pick(&release(&refs), "nope", API).unwrap_err());
        assert!(msg.contains("\"f19\"") && !msg.contains("\"f20\""), "{msg}");
        assert!(msg.contains("and 5 more"), "{msg}");
    }

    #[test]
    fn a_duplicate_name_is_refused() {
        let json = release(&[("a", &format!("{API}/1")), ("a", &format!("{API}/2"))]);
        let msg = format!("{:#}", pick(&json, "a", API).unwrap_err());
        assert!(msg.contains("more than one asset"), "{msg}");
    }

    #[test]
    fn malformed_json_says_what_was_expected() {
        for bad in [&b"not json"[..], br#"{"message":"Not Found"}"#, br#"[]"#] {
            let msg = format!("{:#}", pick(bad, "a", API).unwrap_err());
            assert!(msg.contains("did not return a GitHub release"), "{msg}");
        }
    }

    #[test]
    fn a_url_outside_the_api_is_refused() {
        for url in [
            "https://evil.example/repos/o/r/releases/assets/1",
            "http://api.github.com/repos/o/r/releases/assets/1",
            "https://api.github.com.evil.example/x",
            "https://api.github.com",
            "https://api.github.com/x\nhttps://evil.example/",
            "https://api.github.com/x y",
        ] {
            let json = release(&[("a", url)]);
            assert!(pick(&json, "a", API).is_err(), "{url:?} must be refused");
        }
        // GitHub Enterprise: the API is under a path.
        let ghes = "https://ghe.example/api/v3";
        let json = release(&[(
            "a",
            "https://ghe.example/api/v3/repos/o/r/releases/assets/9",
        )]);
        pick(&json, "a", ghes).unwrap();
        let json = release(&[("a", "https://ghe.example/repos/o/r/releases/assets/9")]);
        pick(&json, "a", ghes).unwrap_err();
    }

    async fn parse(cfg: &[(&str, &str)]) -> anyhow::Result<TargetDef> {
        let config: HashMap<String, Value> = cfg
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect();
        Ok(Driver
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr("//pkg:tool_url")?,
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

    #[tokio::test]
    async fn src_is_a_hashed_input_and_every_option_moves_the_hash() {
        let base = [("src", ":tool_release"), ("asset", "a"), ("api", API)];
        let def = parse(&base).await.unwrap();
        assert_eq!(def.inputs[0].r#ref.r#ref.format(), "//pkg:tool_release");
        assert!(def.inputs[0].hashed);
        assert!(matches!(
            &def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "pkg/tool_url"
        ));
        for (k, v) in [
            ("src", ":other"),
            ("src", ":tool_release|x"),
            ("src", ":tool_release[x.json]"),
            ("asset", "b"),
            ("api", "https://x"),
        ] {
            let mut cfg = base;
            for kv in &mut cfg {
                if kv.0 == k {
                    kv.1 = v;
                }
            }
            assert_ne!(parse(&cfg).await.unwrap().hash, def.hash, "{k}");
        }
        assert!(parse(&[("src", ":r"), ("api", API)]).await.is_err());
        assert!(
            parse(&[("src", ":r"), ("asset", ""), ("api", API)])
                .await
                .is_err()
        );
    }
}
