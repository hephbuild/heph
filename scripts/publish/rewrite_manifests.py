#!/usr/bin/env python3
"""Rewrite the staged crate manifests for a crates.io publish.

crates.io has no notion of "vendor a path dep" — every dependency must resolve
from the registry. So the publish set is shipped as a renamed chain: each crate
is published under a `heph-*` name (the bare names `core`/`model`/`plugin` are
taken/reserved), and every intra-set path dependency gains a `version` so cargo
can record a registry requirement while still resolving locally at verify time.

This operates ONLY on the isolated staging tree (see publish-crates.sh), which
contains exactly the publish set — so every relative path dep points at another
set member and the rewrite is uniform.

Usage: rewrite_manifests.py <staging-dir> <version> <repository-url> <license>
"""

import os
import re
import sys

# old crate name (== its dir under crates/, except proto-gen) -> published name.
RENAME = {
    "core": "heph-core",
    "model": "heph-model",
    "htspec-derive": "heph-htspec-derive",
    "proto-gen": "heph-proto-gen",
    "plugin": "heph-plugin",
    "driver-support": "heph-driver-support",
    "plugin-abi": "heph-plugin-abi",
    "plugin-stabby": "heph-plugin-stabby",
    "plugin-sdk": "heph-plugin-sdk",
}

DESC = {
    "heph-core": "Core primitives (content, hashing, async cancellation) for the heph build engine.",
    "heph-model": "Address, matcher, and package model types for the heph build engine.",
    "heph-htspec-derive": "Derive macros (Spec/SpecStruct/SpecEnum) for heph target-config specs.",
    "heph-proto-gen": "Generated protobuf types for the heph plugin ABI.",
    "heph-plugin": "Provider/Driver trait contract for heph plugins.",
    "heph-driver-support": "Helper types for implementing heph drivers.",
    "heph-plugin-abi": "Wire ABI types for out-of-process and cdylib heph plugins.",
    "heph-plugin-stabby": "Stable-ABI cdylib transport for in-process heph plugins.",
    "heph-plugin-sdk": "SDK for writing third-party heph plugins.",
}

# (manifest path relative to staging, old crate name).
MANIFESTS = [
    ("crates/core/Cargo.toml", "core"),
    ("crates/model/Cargo.toml", "model"),
    ("crates/htspec-derive/Cargo.toml", "htspec-derive"),
    ("crates/plugin/Cargo.toml", "plugin"),
    ("crates/driver-support/Cargo.toml", "driver-support"),
    ("crates/plugin-abi/Cargo.toml", "plugin-abi"),
    ("crates/plugin-stabby/Cargo.toml", "plugin-stabby"),
    ("crates/plugin-sdk/Cargo.toml", "plugin-sdk"),
    ("gen/proto/Cargo.toml", "proto-gen"),
]

# Deps keyed by the bare crate name (no `package = ` field) that nonetheless
# point at a renamed set member — they need an explicit `package = "heph-*"`
# injected so the published manifest names the registry crate, while the table
# key (the code-facing import name) stays put.
BARE_KEYS = {
    "plugin-abi": "heph-plugin-abi",
    "htspec-derive": "heph-htspec-derive",
}


def process(path: str, old: str, version: str, repo: str, license_: str) -> None:
    new = RENAME[old]
    s = open(path, encoding="utf-8").read()

    # [package] name
    s = re.sub(rf'(?m)^name = "{re.escape(old)}"$', f'name = "{new}"', s, count=1)

    # [package] version -> publish version, and append the crates.io-required
    # metadata directly after it.
    meta = (
        f'version = "{version}"\n'
        f'license = "{license_}"\n'
        f'description = "{DESC[new]}"\n'
        f'repository = "{repo}"'
    )
    s = re.sub(r'(?m)^version = "[^"]*"$', meta, s, count=1)

    # Rename every `package = "<old>"` dependency reference.
    for o2, n2 in RENAME.items():
        s = s.replace(f'package = "{o2}"', f'package = "{n2}"')

    # Inject `package = ` for bare-key deps that resolve to a renamed crate.
    for bare, target in BARE_KEYS.items():
        s = re.sub(
            rf'(?m)^{re.escape(bare)} = \{{ ',
            f'{bare} = {{ package = "{target}", ',
            s,
        )

    # Give every intra-set path dep an explicit version requirement.
    s = re.sub(r'(path = "\.\.[^"]*")', rf'\1, version = "{version}"', s)

    open(path, "w", encoding="utf-8").write(s)


def main() -> None:
    stage, version, repo, license_ = sys.argv[1:5]
    for rel, old in MANIFESTS:
        process(os.path.join(stage, rel), old, version, repo, license_)


if __name__ == "__main__":
    main()
