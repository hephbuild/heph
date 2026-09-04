//! Resolving, minting and delivering the credentials a target declared.
//!
//! The split of work here is deliberate and is what keeps the plugin seam
//! narrow: **the host does everything except place the environment.** It reads
//! the declarations, checks the slots, mints the values, writes the files, and
//! scrubs them afterwards. What crosses to the driver is a map of environment
//! variables — mostly *paths*, and values only for a shape that explicitly
//! asked for that.
//!
//! Nothing here is a new ABI surface. A credential reference is an ordinary
//! annotated `Input` (see `hdriver_support::secret`), the same channel scratch
//! uses, so no proto message changed and a third-party driver participates
//! without recompiling.
//!
//! # Why the spec, not the built descriptor
//!
//! Everything this module needs from a `secret()` target — its identity, its
//! slot keys, its acquisition routes — is read from the target's *spec*, never
//! from the `secret.json` it builds. That is not an optimization. A `raw_def`
//! is opaque to the host by contract, and more importantly the checks have to
//! run on a **fully warm build** where every consumer is a cache hit and no
//! descriptor is ever executed. Reading specs is what keeps a slot collision
//! failing identically on every machine, before the first network call.

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use anyhow::Context as _;
use hmodel::htaddr::Addr;
use hplugin::driver::targetdef::Input;
use hsecrets::broker::BrokerCtx;
use hsecrets::descriptor::Descriptor;
use hsecrets::render::{Rendered, Rendering};
use hsecrets::shape::Claim;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A credential a target holds, resolved from its declaration.
pub struct ResolvedSecret {
    /// The consumer's name for it.
    pub name: String,
    pub desc: Descriptor,
    /// The dependency chain that supplied it, empty when declared directly.
    ///
    /// `merge_sandbox` already builds this into the input's `origin_id`, so it
    /// costs nothing to carry — and it is the only thing that makes a policy
    /// failure legible to a target that named none of it.
    pub via: Vec<String>,
}

/// What a target's credentials produced, and how to take it back.
pub(crate) struct SecretDelivery {
    /// Environment for the command: pointer variables, and `env`-shape values.
    pub env: BTreeMap<String, String>,
    /// `(name, value)` for every credential, so the driver can mask them out of
    /// the output it produces.
    pub values: Vec<(String, String)>,
}

impl SecretDelivery {
    /// Remove every rendered credential, leaving a marker in its place.
    ///
    /// Called before a failing target's sandbox is left for diagnostics. That
    /// tree survives until the target's next run — and a crash or SIGKILL
    /// leaves it indefinitely — so without this a failed build is how
    /// credentials end up on disk.
    pub(crate) fn scrub(sandbox_dir: &std::path::Path) {
        let removed = hsecrets::render::scrub_sandbox(sandbox_dir);
        if removed > 0 {
            tracing::debug!(
                removed,
                sandbox = %sandbox_dir.display(),
                "scrubbed credential files from a sandbox kept for diagnostics"
            );
        }
    }
}

impl Engine {
    /// [`Self::resolve_secrets`], for `heph auth`.
    ///
    /// A thin public alias rather than making the real one public: the CLI
    /// genuinely needs to read what a target holds, and naming that need
    /// separately keeps the execute path's version `pub(crate)` where it
    /// belongs.
    pub async fn resolve_secrets_for_check(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        inputs: &[Input],
    ) -> anyhow::Result<Vec<ResolvedSecret>> {
        self.resolve_secrets(rs, consumer, inputs).await
    }

    /// Mint one credential and drop it, for `heph auth check`.
    ///
    /// Nothing is rendered, written or returned — only whether the route
    /// worked. That is deliberately all a check can report: printing the value
    /// is how it reaches scrollback and a pasted bug report, which is why there
    /// is no `heph auth token`.
    pub async fn mint_for_check(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        secret: &ResolvedSecret,
    ) -> anyhow::Result<()> {
        let env_lookup = |k: &str| std::env::var(k).ok();
        let ctx = BrokerCtx {
            now: std::time::SystemTime::now(),
            env: &env_lookup,
            ctoken: rs.ctoken(),
            request_id: rs.request_id(),
            runner: None,
            cwd: &self.cfg.root,
        };
        rs.broker().mint(&secret.desc, &secret.name, &ctx).await?;
        Ok(())
    }

    /// Read every credential declaration a target references.
    ///
    /// Cheap on the overwhelmingly common path: a target with no credential
    /// inputs returns without resolving anything.
    pub(crate) async fn resolve_secrets(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        inputs: &[Input],
    ) -> anyhow::Result<Vec<ResolvedSecret>> {
        let refs: Vec<(&str, &Input)> = inputs
            .iter()
            .filter_map(|i| {
                hdriver_support::secret::secret_name(&i.annotations).map(|name| (name, i))
            })
            .collect();
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let mut resolved = Vec::with_capacity(refs.len());
        for (name, input) in refs {
            let addr = input.r#ref.r#ref.clone();
            let spec = Arc::clone(self)
                .get_spec(rs.clone(), &addr)
                .await
                .with_context(|| {
                    format!("{consumer} references secret {addr}, which does not resolve")
                })?;

            // Naming the wrong kind of target would otherwise surface as a
            // credential that never arrives. Name both ends: the author is
            // looking at the consumer, and the problem is the thing it named.
            if spec.driver != hbuiltins::pluginsecret::DRIVER_NAME {
                anyhow::bail!(
                    "{consumer} lists {addr} under `secrets`, but {addr} is a `{}` target — \
                     `secrets` takes addresses of `{}` targets, which declare how to obtain a \
                     credential. Did you mean to put it in `deps`?",
                    spec.driver,
                    hbuiltins::pluginsecret::DRIVER_NAME,
                );
            }

            let desc = hbuiltins::pluginsecret::parse_declaration(&spec)
                .with_context(|| format!("{consumer} references secret {addr}"))?;
            // `secret|<name>` is a direct declaration; anything longer was
            // rewritten by `merge_sandbox` as it travelled up.
            let via: Vec<String> = input
                .origin_id
                .split('_')
                .filter(|part| !part.is_empty() && *part != "secret")
                .map(str::to_string)
                .collect();
            resolved.push(ResolvedSecret {
                name: name.to_string(),
                desc,
                via: if input.origin_id.starts_with("secret|") {
                    Vec::new()
                } else {
                    via
                },
            });
        }

        check_slots(consumer, &resolved)?;
        check_allow(consumer, &resolved)?;
        Ok(resolved)
    }

    /// Mint every credential and render it into the sandbox.
    ///
    /// Separate from [`Self::resolve_secrets`] because the two happen at
    /// different moments and for different reasons: resolution and the slot
    /// check are declaration-only and must run even when nothing executes,
    /// while minting is the one part that must *not* happen on a cache hit.
    pub(crate) async fn deliver_secrets(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        resolved: &[ResolvedSecret],
        sandbox_dir: &std::path::Path,
    ) -> anyhow::Result<SecretDelivery> {
        if resolved.is_empty() {
            return Ok(SecretDelivery {
                env: BTreeMap::new(),
                values: Vec::new(),
            });
        }

        let broker = rs.broker();
        let env_lookup = |k: &str| std::env::var(k).ok();
        let ctx = BrokerCtx {
            now: std::time::SystemTime::now(),
            env: &env_lookup,
            ctoken: rs.ctoken(),
            request_id: rs.request_id(),
            // Resolving an `acquire` runner address needs a build of its own and
            // is not wired yet; a helper runs local until it is.
            runner: None,
            cwd: &self.cfg.root,
        };

        let mut creds = Vec::with_capacity(resolved.len());
        for r in resolved {
            let cred = broker
                .mint(&r.desc, &r.name, &ctx)
                .await
                .with_context(|| format!("{consumer} needs secret {}", r.desc.addr))?;

            // The audit record. "This token leaked, what now" needs an answer,
            // and the event stream is where it belongs — which descriptor, for
            // which target, by which route, and how long it lives. Never the
            // value and never the subject: a build log is not the place for
            // either, and nothing an incident needs is missing without them.
            //
            // Emitted per consumer rather than per mint, deliberately. The
            // broker dedupes minting, but *who held it* is the question an
            // incident actually asks, and one line per descriptor would answer
            // a different one.
            let route = broker
                .grants()
                .await
                .into_iter()
                .rfind(|g| g.addr == r.desc.addr);
            rs.emit(crate::engine::event::BuildEventKind::SecretGranted {
                addr: consumer.format(),
                secret: r.desc.addr.clone(),
                name: r.name.clone(),
                // From the grant, not from `acquire[0]`: a descriptor with two
                // routes reports whichever one actually ran, which is the whole
                // reason the route is recorded.
                provider: route
                    .as_ref()
                    .map(|g| provider_name(g.provider))
                    .unwrap_or("unknown")
                    .to_string(),
                acquire_index: route.as_ref().map(|g| g.acquire_index).unwrap_or_default(),
                selected_by: route.as_ref().and_then(|g| g.selected_by.clone()),
                ttl_secs: cred.expiry.usable_for(ctx.now).as_secs(),
                expiry_source: cred.expiry.source.as_str().to_string(),
            });

            creds.push(cred);
        }

        let renderings: Vec<Rendering<'_>> = resolved
            .iter()
            .zip(creds.iter())
            .map(|(r, cred)| Rendering {
                name: &r.name,
                identity: &r.desc.identity,
                cred,
            })
            .collect();

        // `files` is deliberately dropped: the scrub walks the sandbox instead,
        // so it also catches whatever a driver wrote into the synthetic home on
        // its own. A recorded list would only ever be a subset.
        let Rendered { env, values, .. } =
            hsecrets::render::render_all(sandbox_dir, &renderings)
                .with_context(|| format!("render credentials for {consumer}"))?;

        tracing::debug!(
            target = %consumer,
            count = resolved.len(),
            "delivered credentials"
        );
        Ok(SecretDelivery { env, values })
    }
}

/// The stable spelling of a provider, for the event stream.
///
/// A `Debug` rendering would be `StaticEnv` and would silently change if the
/// variant were ever renamed — and this is a consumer-facing field.
fn provider_name(kind: hsecrets::descriptor::ProviderKind) -> &'static str {
    match kind {
        hsecrets::descriptor::ProviderKind::StaticEnv => "static_env",
        hsecrets::descriptor::ProviderKind::Exec => "exec",
        hsecrets::descriptor::ProviderKind::Oidc => "oidc",
    }
}

/// Reject a target that is not permitted to hold a credential it references.
///
/// **Evaluated on the effective set** — what a target holds after
/// `apply_transitive`, not what it declared. Anything else lets a dependency
/// launder a credential past its own policy: the consumer names nothing, and
/// the check that was supposed to stop it never sees the edge.
///
/// The failure is legitimate even when the consumer wrote nothing, so the
/// message carries the chain that supplied it. Without that a reader is told
/// their target may not hold a credential they have never heard of.
fn check_allow(consumer: &Addr, resolved: &[ResolvedSecret]) -> anyhow::Result<()> {
    for r in resolved {
        let Some(query) = r.desc.allow.as_deref().filter(|q| !q.trim().is_empty()) else {
            continue;
        };
        let matcher = hmodel::htquery::parse(query, &consumer.package).with_context(|| {
            format!(
                "secret {}: `allow` is not a valid target query: {query:?}",
                r.desc.addr
            )
        })?;
        if matches!(
            matcher.matches_addr(consumer),
            hmodel::htmatcher::MatchResult::MatchYes
        ) {
            continue;
        }
        let via = if r.via.is_empty() {
            String::new()
        } else {
            format!("\n  It reached this target through {}.", r.via.join(" → "))
        };
        anyhow::bail!(
            "{consumer} is not permitted to hold secret {} (as {:?}).{via}\n  Its `allow` is \
             {query:?}, and {consumer} does not match.\n  Widen `allow` on {}, or stop \
             depending on the credential.",
            r.desc.addr,
            r.name,
            r.desc.addr,
        );
    }
    Ok(())
}

/// Reject a set of credentials that would fight over one file or variable.
///
/// Runs from declarations alone, so it fails identically on every machine and
/// before the first network call — and, crucially, on a warm build where
/// nothing is minted at all.
fn check_slots(consumer: &Addr, resolved: &[ResolvedSecret]) -> anyhow::Result<()> {
    // A name is what the command references, so two credentials answering to one
    // name is worse than a shape collision: picking either changes what
    // `$SECRET_<NAME>` resolves to without touching a line the author can see.
    let mut by_name: BTreeMap<&str, &str> = BTreeMap::new();
    for r in resolved {
        if let Some(first) = by_name.insert(&r.name, &r.desc.addr)
            && first != r.desc.addr
        {
            anyhow::bail!(
                "{consumer} declares two different secrets named {:?}: {first} and {}. The name \
                 is what the command references as `$SECRET_<NAME>`, so one would silently win.",
                r.name,
                r.desc.addr
            );
        }
    }

    let claims: Vec<Claim> = resolved
        .iter()
        .map(|r| Claim {
            name: r.name.clone(),
            addr: r.desc.addr.clone(),
            via: r.via.clone(),
            identity: r.desc.identity.clone(),
        })
        .collect();
    hsecrets::shape::check_collisions(&consumer.to_string(), &claims)
}
