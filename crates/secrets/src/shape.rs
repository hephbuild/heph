//! Shapes: how a minted value reaches the tool that needs it.
//!
//! # Delivery is a file, not an environment variable
//!
//! `$SECRET_<NAME>` holds a *path*, mode 0600, under `<sandbox>/secrets/` —
//! outside `ws/`, so an `out = ["**"]` can never sweep it into an artifact.
//! `environ` is readable through `/proc/<pid>/environ` by any same-uid process
//! and is inherited by every descendant, postinstall scripts included;
//! systemd's `$CREDENTIALS_DIRECTORY` and BuildKit's `--mount=type=secret` both
//! landed on files for exactly this reason.
//!
//! Most tools want something else, so a descriptor names a shape, and the shape
//! renders a well-known file *plus* the pointer variable that aims the tool at
//! it.
//!
//! # A shape is a slot in a shared namespace, not a private file
//!
//! Two credentials on one target that both render `netrc` want the same
//! `<sandbox>/home/.netrc`. Letting the second quietly overwrite the first is
//! the worst available outcome: the build succeeds, one identity silently
//! vanishes, and which one survives depends on map iteration order.
//!
//! But refusing outright is wrong too — a `.netrc` is *designed* to hold one
//! entry per machine. So each shape declares a **merge key**, taken from the
//! descriptor and therefore known before anything is minted, which gives one
//! rule for the whole feature:
//!
//! > A shape contributes entries to keyed files and variables to the
//! > environment. Both are namespaces. Distinct keys merge; the same key with
//! > differing values is an error naming both descriptors.
//!
//! **The check runs from the secret targets' specs, not their built output.**
//! Every merge key here is an attribute of the `secret()` declaration, so the
//! engine can read it with `get_spec` without building or minting anything.
//! That matters because the obvious alternative does not work: `Engine::link`
//! filters `runtime: false` inputs out entirely, so a descriptor input never
//! reaches link at all. Reading the spec keeps the property that matters — a
//! collision fails identically on every machine, before the first network call,
//! and before a cache hit can skip the question.

use crate::descriptor::Identity;
use std::collections::BTreeMap;
use std::fmt;

/// The well-known forms a credential can be rendered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    /// The default: a 0600 file, its path in `$SECRET_<NAME>`.
    File,
    /// Named environment variables. An explicit, per-secret opt-in to the
    /// exposure — see [`Shape::leaks_via_argv`].
    Env,
    /// `<home>/.netrc`, one entry per machine.
    Netrc,
    /// `<home>/.docker/config.json`, one entry per registry under `auths`.
    DockerConfig,
    /// `<home>/.git-credentials` plus the `GIT_CONFIG_*` helper wiring.
    GitCredential,
    /// `<home>/.aws/credentials` and `<home>/.aws/config`, one section each.
    AwsProfile,
    /// `<home>/.config/gcloud/application_default_credentials.json`. A
    /// singleton: `GOOGLE_APPLICATION_CREDENTIALS` names one identity.
    GcloudAdc,
}

impl Shape {
    pub fn parse(s: &str) -> anyhow::Result<Shape> {
        Ok(match s {
            "file" => Shape::File,
            "env" => Shape::Env,
            "netrc" => Shape::Netrc,
            "docker_config" => Shape::DockerConfig,
            "git_credential" => Shape::GitCredential,
            "aws_profile" => Shape::AwsProfile,
            "gcloud_adc" => Shape::GcloudAdc,
            other => anyhow::bail!(
                "unknown shape {other:?}. Known shapes: file, env, netrc, docker_config, \
                 git_credential, aws_profile, gcloud_adc"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Shape::File => "file",
            Shape::Env => "env",
            Shape::Netrc => "netrc",
            Shape::DockerConfig => "docker_config",
            Shape::GitCredential => "git_credential",
            Shape::AwsProfile => "aws_profile",
            Shape::GcloudAdc => "gcloud_adc",
        }
    }

    /// Whether this shape puts a credential *value* into the environment.
    ///
    /// The oci runner passes a target's whole environment to the docker CLI as
    /// `docker exec -e KEY=VALUE` **argv**, and on Linux `/proc/<pid>/cmdline`
    /// is world-readable — so a value shape sits on a command line any local
    /// uid can read for the duration of the exec. Pointer variables are
    /// harmless, because they are paths. This predicate is what lets the runner
    /// seam refuse the difference rather than leaking it silently: the "delivery
    /// works under every runner" test passes while that leak happens.
    pub fn leaks_via_argv(self) -> bool {
        matches!(self, Shape::Env)
    }

    /// Where in the sandbox this shape writes, relative to the synthetic home.
    ///
    /// `None` for shapes that write under `<sandbox>/secrets/` instead.
    pub fn home_path(self) -> Option<&'static str> {
        match self {
            Shape::File | Shape::Env => None,
            Shape::Netrc => Some(".netrc"),
            Shape::DockerConfig => Some(".docker/config.json"),
            Shape::GitCredential => Some(".git-credentials"),
            Shape::AwsProfile => Some(".aws/credentials"),
            Shape::GcloudAdc => Some(".config/gcloud/application_default_credentials.json"),
        }
    }

    /// The descriptor field this shape's merge key is drawn from, for the
    /// diagnostic that has to tell an author what to change.
    pub fn key_field(self) -> &'static str {
        match self {
            Shape::File => "name",
            Shape::Env => "env",
            Shape::Netrc | Shape::GitCredential => "machine",
            Shape::DockerConfig => "registry",
            Shape::AwsProfile => "profile",
            Shape::GcloudAdc => "(singleton)",
        }
    }

    /// The merge keys this shape claims for a given descriptor.
    ///
    /// Several for `env` (one per variable), exactly one for everything else.
    /// An error here means the descriptor cannot render this shape at all —
    /// `netrc` with no `machine` has nowhere to put its entry.
    pub fn slots(self, name: &str, identity: &Identity) -> anyhow::Result<Vec<Slot>> {
        let need = |v: &Option<String>, field: &str| -> anyhow::Result<String> {
            v.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "shape {:?} needs `{field}` on the descriptor: it is the key of the entry \
                     this shape writes, and without it two credentials would silently claim the \
                     same slot",
                    self.as_str()
                )
            })
        };
        Ok(match self {
            // Never collides: each secret gets its own path and its own
            // `$SECRET_<NAME>`.
            Shape::File => vec![Slot::new(self, name.to_string())],
            Shape::Env => {
                if identity.env.is_empty() {
                    anyhow::bail!(
                        "shape \"env\" needs an `env` map naming the variables to set, e.g. \
                         env = {{\"GH_TOKEN\": \"$.token\"}}"
                    );
                }
                identity
                    .env
                    .keys()
                    .map(|v| Slot::new(self, v.clone()))
                    .collect()
            }
            Shape::Netrc | Shape::GitCredential => {
                vec![Slot::new(self, need(&identity.machine, "machine")?)]
            }
            Shape::DockerConfig => vec![Slot::new(self, need(&identity.registry, "registry")?)],
            Shape::AwsProfile => vec![Slot::new(
                self,
                identity.profile.clone().unwrap_or_else(|| {
                    // Both defaulting to `default` is a collision, and it is the
                    // one this document's own AWS and Cloudflare examples hit.
                    // Defaulting rather than requiring keeps the single-secret
                    // case pleasant; the collision check is what catches the
                    // rest.
                    "default".to_string()
                }),
            )],
            // A singleton. `GOOGLE_APPLICATION_CREDENTIALS` names one identity,
            // so the second one on a target is always a collision.
            Shape::GcloudAdc => vec![Slot::new(self, String::new())],
        })
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One claimed slot: a shape plus the key within it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    pub shape: Shape,
    /// Empty for singleton shapes.
    pub key: String,
}

impl Slot {
    pub fn new(shape: Shape, key: String) -> Self {
        Self { shape, key }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_empty() {
            write!(f, "{}", self.shape)
        } else {
            write!(f, "{} {}", self.shape, self.key)
        }
    }
}

/// One secret as a target effectively holds it, whether declared or inherited.
#[derive(Debug, Clone)]
pub struct Claim {
    /// The name the command references as `$SECRET_<NAME>`.
    pub name: String,
    /// The descriptor target's address.
    pub addr: String,
    /// The chain of dependencies that supplied it, empty when declared
    /// directly. `merge_sandbox` already rewrites ids into exactly this, so a
    /// collision between two deps the author never named can still say where
    /// each came from.
    pub via: Vec<String>,
    pub identity: Identity,
}

impl Claim {
    fn provenance(&self) -> String {
        if self.via.is_empty() {
            format!("{} (declared)", self.addr)
        } else {
            format!("{} (via {})", self.addr, self.via.join(" → "))
        }
    }
}

/// Check every shape slot on one target for collisions.
///
/// Runs at spec time, from declarations alone — no build, no mint, no network.
pub fn check_collisions(target: &str, claims: &[Claim]) -> anyhow::Result<()> {
    // Slot → the claims that want it.
    let mut by_slot: BTreeMap<Slot, Vec<&Claim>> = BTreeMap::new();

    for claim in claims {
        for shape_name in &claim.identity.shape {
            let shape = Shape::parse(shape_name).map_err(|e| {
                anyhow::anyhow!("{target}: secret {:?} ({}): {e}", claim.name, claim.addr)
            })?;
            let slots = shape.slots(&claim.name, &claim.identity).map_err(|e| {
                anyhow::anyhow!("{target}: secret {:?} ({}): {e}", claim.name, claim.addr)
            })?;
            for slot in slots {
                by_slot.entry(slot).or_default().push(claim);
            }
        }
    }

    for (slot, wanters) in by_slot {
        if wanters.len() < 2 {
            continue;
        }
        // Identical descriptors are idempotent and merge silently — which is
        // what makes two deps sharing one credential uneventful. It is *the
        // same descriptor address* that makes them identical, not equal
        // identities: two addresses with equal identities are still two
        // credentials, and which value lands would be arbitrary.
        let first = wanters.first().map(|c| c.addr.as_str()).unwrap_or_default();
        if wanters.iter().all(|c| c.addr == first) {
            continue;
        }

        let mut detail = String::new();
        for c in &wanters {
            detail.push_str(&format!(
                "\n  {:<8} {:<28} shape {:<14} {} = {}",
                c.name,
                c.provenance(),
                slot.shape.as_str(),
                slot.shape.key_field(),
                if slot.key.is_empty() {
                    "(singleton)"
                } else {
                    slot.key.as_str()
                },
            ));
        }
        let where_ = slot
            .shape
            .home_path()
            .map(|p| format!("<sandbox>/home/{p}"))
            .unwrap_or_else(|| "the target's environment".to_string());

        anyhow::bail!(
            "{target} declares {} secrets writing {} into {where_}{detail}\n\n  One would \
             silently win, and the tool would use whichever it was.\n  fix    give each a \
             distinct `{}`, and select it per command; or use shape = \"file\" and read \
             $SECRET_<NAME> directly.",
            wanters.len(),
            if slot.key.is_empty() {
                slot.shape.as_str().to_string()
            } else {
                format!("{} {:?}", slot.shape.key_field(), slot.key)
            },
            slot.shape.key_field(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(name: &str, addr: &str, shapes: &[&str], identity: Identity) -> Claim {
        Claim {
            name: name.to_string(),
            addr: addr.to_string(),
            via: Vec::new(),
            identity: Identity {
                shape: shapes.iter().map(|s| (*s).to_string()).collect(),
                ..identity
            },
        }
    }

    #[test]
    fn distinct_machines_merge_into_one_netrc() {
        let claims = vec![
            claim(
                "gh",
                "//c:gh",
                &["netrc"],
                Identity {
                    machine: Some("github.com".into()),
                    ..Identity::default()
                },
            ),
            claim(
                "gl",
                "//c:gl",
                &["netrc"],
                Identity {
                    machine: Some("gitlab.com".into()),
                    ..Identity::default()
                },
            ),
        ];
        check_collisions("//svc:build", &claims).expect("distinct machines merge");
    }

    #[test]
    fn the_same_machine_from_two_descriptors_is_an_error_naming_both() {
        let claims = vec![
            claim(
                "a",
                "//c:a",
                &["netrc"],
                Identity {
                    machine: Some("github.com".into()),
                    ..Identity::default()
                },
            ),
            claim(
                "b",
                "//c:b",
                &["netrc"],
                Identity {
                    machine: Some("github.com".into()),
                    ..Identity::default()
                },
            ),
        ];
        let err = check_collisions("//svc:build", &claims).expect_err("collision");
        let msg = err.to_string();
        assert!(msg.contains("//c:a"), "{msg}");
        assert!(msg.contains("//c:b"), "{msg}");
        assert!(msg.contains("github.com"), "{msg}");
        assert!(msg.contains(".netrc"), "{msg}");
    }

    /// The collision this design's own examples contain: ECR and R2 both
    /// rendering `aws_profile` into `[default]`.
    #[test]
    fn two_aws_profiles_both_defaulting_collide() {
        let claims = vec![
            claim(
                "ecr",
                "//infra/creds:ecr",
                &["aws_profile"],
                Identity::default(),
            ),
            claim(
                "r2",
                "//infra/creds:r2",
                &["aws_profile"],
                Identity::default(),
            ),
        ];
        let err = check_collisions("//svc:release", &claims).expect_err("collision");
        let msg = err.to_string();
        assert!(msg.contains("profile"), "{msg}");
        assert!(msg.contains("//infra/creds:ecr"), "{msg}");
        assert!(msg.contains("//infra/creds:r2"), "{msg}");

        // Naming them fixes it, which is the fix the message recommends.
        let fixed = vec![
            claim(
                "ecr",
                "//infra/creds:ecr",
                &["aws_profile"],
                Identity {
                    profile: Some("ecr".into()),
                    ..Identity::default()
                },
            ),
            claim(
                "r2",
                "//infra/creds:r2",
                &["aws_profile"],
                Identity {
                    profile: Some("r2".into()),
                    ..Identity::default()
                },
            ),
        ];
        check_collisions("//svc:release", &fixed).expect("named profiles merge");
    }

    /// Two deps needing the same credential is the common case, not a conflict.
    #[test]
    fn the_same_descriptor_arriving_twice_is_idempotent() {
        let mut a = claim(
            "github",
            "//infra/creds:github",
            &["netrc"],
            Identity {
                machine: Some("github.com".into()),
                ..Identity::default()
            },
        );
        let mut b = a.clone();
        a.via = vec!["//lib:a".into()];
        b.via = vec!["//lib:b".into()];
        check_collisions("//svc:build", &[a, b]).expect("same descriptor dedupes");
    }

    /// gcloud_adc names one identity, so the second is always a collision — and
    /// the message must say so without a key, since there is none.
    #[test]
    fn gcloud_adc_is_a_singleton() {
        let claims = vec![
            claim("a", "//c:a", &["gcloud_adc"], Identity::default()),
            claim("b", "//c:b", &["gcloud_adc"], Identity::default()),
        ];
        let err = check_collisions("//svc:x", &claims).expect_err("singleton");
        assert!(err.to_string().contains("gcloud_adc"), "{err}");
    }

    #[test]
    fn env_collides_per_variable_name_not_per_secret() {
        let one = claim(
            "a",
            "//c:a",
            &["env"],
            Identity {
                env: BTreeMap::from([("GH_TOKEN".to_string(), "$.".to_string())]),
                ..Identity::default()
            },
        );
        let two = claim(
            "b",
            "//c:b",
            &["env"],
            Identity {
                env: BTreeMap::from([("GL_TOKEN".to_string(), "$.".to_string())]),
                ..Identity::default()
            },
        );
        check_collisions("//x:y", &[one.clone(), two]).expect("distinct variables merge");

        let clash = claim(
            "b",
            "//c:b",
            &["env"],
            Identity {
                env: BTreeMap::from([("GH_TOKEN".to_string(), "$.".to_string())]),
                ..Identity::default()
            },
        );
        let err = check_collisions("//x:y", &[one, clash]).expect_err("same variable");
        assert!(err.to_string().contains("GH_TOKEN"), "{err}");
    }

    /// `file` gives every secret its own path, so it is the escape hatch the
    /// collision message recommends and must never collide.
    #[test]
    fn file_shape_never_collides() {
        let claims = vec![
            claim("a", "//c:a", &["file"], Identity::default()),
            claim("b", "//c:b", &["file"], Identity::default()),
        ];
        check_collisions("//x:y", &claims).expect("file never collides");
    }

    #[test]
    fn a_shape_missing_its_key_field_fails_with_the_field_named() {
        let claims = vec![claim("a", "//c:a", &["netrc"], Identity::default())];
        let err = check_collisions("//x:y", &claims).expect_err("no machine");
        assert!(err.to_string().contains("`machine`"), "{err}");
    }

    #[test]
    fn only_the_env_shape_leaks_through_argv() {
        assert!(Shape::Env.leaks_via_argv());
        for s in [
            Shape::File,
            Shape::Netrc,
            Shape::DockerConfig,
            Shape::GitCredential,
            Shape::AwsProfile,
            Shape::GcloudAdc,
        ] {
            assert!(!s.leaks_via_argv(), "{s} claimed to leak via argv");
        }
    }

    #[test]
    fn shape_names_round_trip() {
        for s in [
            Shape::File,
            Shape::Env,
            Shape::Netrc,
            Shape::DockerConfig,
            Shape::GitCredential,
            Shape::AwsProfile,
            Shape::GcloudAdc,
        ] {
            assert_eq!(Shape::parse(s.as_str()).expect("round trip"), s);
        }
        Shape::parse("kubeconfig").expect_err("unknown shapes are rejected");
    }
}
