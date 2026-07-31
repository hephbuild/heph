//! The `js` provider + driver family: discovers JS/TS packages across pnpm
//! and npm workspaces.
//!
//! **M0+M1 scope** (see `ai-docs/js-plugin-plan.md`): `package.json` walk,
//! pnpm/npm workspace-member discovery, lockfile parsing
//! (`package-lock.json`/`pnpm-lock.yaml`) into a manager-agnostic resolved
//! dependency graph, the hermetic per-`(name, version, integrity)`
//! `js_install` fetch target, and `js_package_info` wiring a package's
//! declared dependencies (workspace-internal siblings and third-party
//! `js_install` addrs) into its target-dep closure. Type-checking, testing,
//! linting, and bundling are later milestones — this crate does not
//! implement them yet, and the import-graph resolver (oxc) that would let
//! dependency wiring go beyond `package.json`'s declared deps is M2.

pub mod pluginjs;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
