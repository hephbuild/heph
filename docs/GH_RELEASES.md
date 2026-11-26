# GitHub release assets

`heph.gh.release_asset(...)` makes a GitHub release asset a target, optionally
unpacked. It is a function of the `gh` plugin (`crates/plugin-gh`, shipped as
the `plugin-gh-cdylib` cdylib), so the plugin must be enabled in `.hephconfig`.

```python
gh = heph.gh.release_asset(
    name = "gh",
    repo = "cli/cli",
    tag = "v2.40.0",
    asset = "gh_2.40.0_linux_amd64.tar.gz",
    sha256 = "…",
    extract = True,
    strip_components = 1,
)
# gh == "//third_party/gh:gh", a directory holding bin/gh, share/…
```

| Argument | Default | Meaning |
|---|---|---|
| `name` | required | The target name; the function returns `//<pkg>:<name>`. |
| `repo` | required | `owner/name`, letters, digits, `.`, `_`, `-`. |
| `tag` | required | The release tag. May contain `/`. |
| `asset` | required | The asset's file name. |
| `sha256` | none | Forwarded to `http_fetch`. Without it the fetch is unverified and `http_fetch` warns. |
| `extract` | `False` | Unpack the asset. |
| `format` | inferred | Archive format, with `extract` only: `tar`, `tar.gz` (`.tgz`), `tar.zst` (`.tzst`), `zip`. Inferred from `asset`'s suffix, case-insensitively; an unknown suffix needs `format =`. |
| `strip_components` | `0` | With `extract` only: leading path components dropped, as `tar --strip-components`. |
| `executable` | `False` | Without `extract` only: mark the fetched file executable. |
| `out` | see below | Without `extract`: the fetched file's name (default `asset`). With it: the output directory (default `name`). |
| `server_url` | `https://github.com` | The GitHub server the release is on; see below. |

## What it declares

The function does no HTTP itself; it declares targets in the calling package,
exactly as if the BUILD file had written them, and the engine builds them:

- `extract = False`: one `http_fetch` target named `name`.
- `extract = True`: an `http_fetch` named `<name>_archive` (its output is the
  archive, named `asset`), and a `gh.extract` named `name` whose `src` is it.

The URL is `{server_url}/{repo}/releases/download/{tag}/{asset}`. The tag is
percent-encoded per `/`-separated segment, keeping its `/`s (GitHub's own
download URLs do); the asset is one encoded segment. Braces are escaped, so
`http_fetch` does not read them as `{arg}` placeholders.

Fetching, verification and caching are `http_fetch`'s (see its driver docs).
The extracted tree's cache key comes from the archive's content through the
dep, plus `format`, `strip_components` and `out`.

## `server_url`

An argument of each call, default `https://github.com`. Pass it for a GitHub
Enterprise server:

```python
heph.gh.release_asset(
    name = "tool",
    repo = "platform/tool",
    tag = "v1.2.0",
    asset = "tool-linux-amd64.tar.gz",
    server_url = "https://github.example.com",
    extract = True,
)
```

It is per call, not a plugin option, because one workspace can pull from more
than one server. A workspace whose own code lives on a GitHub Enterprise server
still fetches a public tool from github.com by leaving it out:

```python
heph.gh.release_asset(name = "gh", repo = "cli/cli", tag = "v2.40.0",
                      asset = "gh_2.40.0_linux_amd64.tar.gz", extract = True)
```

It must be `https://` or `http://`, a host and an optional path, with no query,
fragment or whitespace; trailing `/`s are dropped. Anything else fails the call
naming `server_url`. The URL is in the declared fetch's config, so it is part of
that target's cache key: the same asset from two servers is two cache entries.

The plugin's only options are its Actions hook's, under `hook:`, and the hook
is built only with `hook: {enable: true}` (`docs/GHA_REPORTING.md` §12);
enabling the plugin for release assets needs no `options:` and loads no hook.

## Unpacking rules

`gh.extract` unpacks in-process (no host `tar`/`unzip`, through the `archive`
crate, `crates/archive`), with the same code on every supported platform. One
difference is the filesystem's: two entries whose
names differ only by case (or Unicode normalization) both extract on Linux,
while on macOS's case-insensitive APFS they are one file, and the second entry
fails with an error saying it collides with the first on a case-insensitive
filesystem. The archive is untrusted, so:

- An absolute path or a `..` component is refused, and so is an entry written
  through a symlink the archive created.
- A symlink must stay inside the output: an absolute target is refused, and
  every link is resolved through the extracted tree, following the links it
  meets, and refused if it climbs above it. A hard link becomes a copy of an
  entry already extracted.
- Device and fifo entries are refused. Paths must be UTF-8.
- Modes are normalized: `0o755` when any execute bit is set, else `0o644`;
  directories `0o755`. Owners and mtimes are not applied.
- A path that appears twice is an error, except a directory listed again. A
  zip whose central directory names a path twice is refused as a whole.
- The output is bounded: at most 4 GiB written and 1,000,000 entries by
  default, counted while unpacking; `max_bytes` / `max_entries` on
  `gh.extract` raise them. They are not hashed: exceeding one fails the run,
  and a failure is never cached.
- `strip_components` counts a leading `.` as GNU tar and bsdtar do
  (`./tool-1.0/bin/tool` with 1 is `tool-1.0/bin/tool`); a hard link's target
  is stripped too, a symlink's is not. An entry with no more components than
  `strip_components` is skipped; an archive from which nothing remains is an
  error.
- The extracted tree is cached (`cache = False` turns it off): it is a pure
  function of the archive's bytes.

## Not supported yet

Private repositories. Their assets are fetched through the GitHub API by asset
id with a token, which needs the asset id resolved first. A later change adds a
`credentials` argument for that; today only public assets work.
