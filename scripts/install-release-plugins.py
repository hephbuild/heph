#!/usr/bin/env python3
"""Install every plugin published in a heph release into the user-global plugin dir.

The `install-<name>-plugin` devenv scripts build a cdylib from *this tree* and
write a manifest whose artifact is a sibling file on disk. That is the right
thing while developing a plugin, and the wrong thing for an installed `heph`:
the host and the cdylib are linked at load time through stabby, so a plugin
built from a different commit fails the ABI check with a wall of type-report
text and no hint about the cause.

This script takes the other route — it installs the manifests published
*alongside the binary that is running*, so host and plugins always come from one
build. A released manifest points at the release assets by `url:` with a
`sha256:` per os/arch, so no cdylib is downloaded here: heph pulls and verifies
the one matching the host on first load (`heph tool resolve-plugins` forces it).

Which plugins get installed is not a list kept here — it is whatever the release
actually published (`heph-<name>-plugin.json` assets), so a new plugin needs no
change to this script.

Usage:
    install-release-plugins [--version VER] [--dest DIR] [--bin HEPH] [--dry-run]

The version defaults to what `heph version` reports; `--bin` picks a different
binary to ask, and `--version` skips asking entirely. Set GITHUB_TOKEN (or
GH_TOKEN) to lift the unauthenticated GitHub API rate limit.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import typing
import urllib.error
import urllib.parse
import urllib.request

REPO = "hephbuild/heph-artifacts-v1"
API = f"https://api.github.com/repos/{REPO}/releases/tags/"
# `heph-<name>-plugin.json` — the `.sha256` sidecar is fetched by name, not matched.
MANIFEST_RE = re.compile(r"^heph-([a-z0-9][a-z0-9-]*)-plugin\.json$")


def die(msg: str, *hints: str) -> typing.NoReturn:
    print(f"error: {msg}", file=sys.stderr)
    for hint in hints:
        print(f"  hint: {hint}", file=sys.stderr)
    sys.exit(1)


def heph_version(binary: str) -> str:
    """Ask a heph binary what release it is."""
    try:
        out = subprocess.run(
            [binary, "version"], capture_output=True, text=True, check=True
        ).stdout.strip()
    except FileNotFoundError:
        die(
            f"`{binary}` not found on PATH",
            "pass --bin to name the binary, or --version to skip asking it",
        )
    except subprocess.CalledProcessError as e:
        die(f"`{binary} version` failed: {e.stderr.strip() or e}")
    if not out:
        die(f"`{binary} version` printed nothing")
    return out


def check_version(version: str, binary: str) -> None:
    """Reject a version that no release can exist for.

    A source build reports `v0.0.0-dev`. Installing release plugins for it would
    404 here, and — worse — installing plugins from some *other* release would
    load a mismatched ABI. Either way the answer is to build the plugins from
    the same tree instead.
    """
    if version.startswith("v0.0.0"):
        die(
            f"`{binary}` is a local build ({version}), not a release",
            "its plugins must come from the same tree: run install-go-plugin / "
            "install-devenv-plugin / install-oci-plugin / install-gha-plugin",
            "or install a released binary first, then re-run this",
        )


def fetch(url: str, accept: str | None = None) -> bytes:
    req = urllib.request.Request(url)
    if accept:
        req.add_header("Accept", accept)
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            die(f"not found: {url}")
        if e.code in (403, 429):
            die(
                f"GitHub refused the request ({e.code}): {url}",
                "set GITHUB_TOKEN or GH_TOKEN to lift the anonymous rate limit",
            )
        die(f"GET {url} failed: {e}")
    except urllib.error.URLError as e:
        die(f"GET {url} failed: {e.reason}")


def release_assets(version: str) -> dict[str, str]:
    """Map asset name -> download URL for one release tag."""
    # The tag carries `+` and `.` (`v1.0.0-alpha-build.391.1625+g0a03fc79`); both
    # are legal in a path segment, but `+` must not be left for a query parser.
    url = API + urllib.parse.quote(version, safe="")
    body = json.loads(fetch(url, accept="application/vnd.github+json"))
    return {a["name"]: a["browser_download_url"] for a in body.get("assets", [])}


def install_one(name: str, assets: dict[str, str], dest: str, dry_run: bool) -> str:
    manifest_name = f"heph-{name}-plugin.json"
    sidecar_name = manifest_name + ".sha256"
    manifest = fetch(assets[manifest_name])
    digest = "sha256:" + hashlib.sha256(manifest).hexdigest()

    # The sidecar is the manifest's own checksum — the value a workspace pins as
    # `plugins[].checksum`. Verify it here so a truncated download is caught now
    # rather than as an unexplained load failure later.
    if sidecar_name in assets:
        want = fetch(assets[sidecar_name]).decode().strip()
        if want != digest:
            die(
                f"{manifest_name}: checksum mismatch",
                f"published {want}, downloaded {digest}",
            )
    else:
        want = digest

    plugin_dir = os.path.join(dest, name)
    if dry_run:
        return plugin_dir

    os.makedirs(plugin_dir, exist_ok=True)
    # A dev install left a cdylib next to the manifest and a manifest pointing at
    # it by `path`. The new manifest resolves by `url`, so that file becomes dead
    # weight — say so rather than deleting someone's build artifact.
    stale = [
        f
        for f in os.listdir(plugin_dir)
        if f.endswith((".so", ".dylib"))
    ]
    for f in stale:
        print(f"  note: {os.path.join(plugin_dir, f)} is now unused (built locally)")

    for fname, data in ((manifest_name, manifest), (sidecar_name, want.encode() + b"\n")):
        path = os.path.join(plugin_dir, fname)
        # Write-then-rename: a half-written manifest is a load failure, and this
        # may be overwriting the manifest of a plugin another process is loading.
        tmp = path + ".new"
        with open(tmp, "wb") as fh:
            fh.write(data)
        os.replace(tmp, path)
    return plugin_dir


def main() -> None:
    ap = argparse.ArgumentParser(
        prog="install-release-plugins",
        description="Install every plugin published in a heph release.",
    )
    ap.add_argument(
        "--bin",
        default=os.environ.get("HEPH_BIN", "heph"),
        help="binary to ask for the version (default: $HEPH_BIN, else `heph`)",
    )
    ap.add_argument("--version", help="release tag to install from (default: `<bin> version`)")
    ap.add_argument(
        "--dest",
        default=os.path.join(os.path.expanduser("~"), ".heph", "plugins"),
        help="plugin root to install into (default: ~/.heph/plugins)",
    )
    ap.add_argument("--dry-run", action="store_true", help="print what would be installed")
    args = ap.parse_args()

    version = args.version
    if version is None:
        version = heph_version(args.bin)
        check_version(version, args.bin)
        print(f"{args.bin} reports {version}")

    assets = release_assets(version)
    names = sorted(
        {m.group(1) for m in (MANIFEST_RE.match(a) for a in assets) if m}
    )
    if not names:
        die(
            f"release {version} publishes no plugin manifests",
            f"assets found: {', '.join(sorted(assets)) or '(none)'}",
        )

    for name in names:
        where = install_one(name, assets, args.dest, args.dry_run)
        verb = "would install" if args.dry_run else "installed"
        print(f"  {verb} {name:<8} -> {where}")

    if not args.dry_run:
        print(
            f"{len(names)} plugin(s) from {version}. The cdylibs download on first "
            "load — `heph tool resolve-plugins` forces it now."
        )


if __name__ == "__main__":
    main()
