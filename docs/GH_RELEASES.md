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
| `strip_components` | `0` | With `extract` only: leading path components dropped, as `tar --strip-components`. Usually `1` for a GitHub tarball, which holds one top-level directory. |
| `max_bytes` | 4 GiB | With `extract` only: the most bytes the archive may unpack to. |
| `max_entries` | 1,000,000 | With `extract` only: the most entries the archive may hold. |
| `executable` | `False` | Without `extract` only: mark the fetched file executable. |
| `out` | see below | Without `extract`: the fetched file's name (default `asset`). With it: the output directory (default `name`). See [`out`](#out). |
| `credentials` | none | Credential targets (`docs/CREDENTIALS.md`) for a private repository: the asset is fetched through the API. See [Private repositories](#private-repositories). |
| `server_url` | `https://github.com` | The GitHub server the release is on. See [`server_url` and `api_url`](#server_url-and-api_url). |
| `api_url` | derived from `server_url` | Its REST API, where a private asset is looked up and fetched. |
| `cache` | local and remote; the release JSON local only | Caching for the asset (fetched, and unpacked with `extract`) and, with `credentials`, the release JSON. The shape every driver's `cache` takes: `False` turns off both tiers, `{"remote": False}` keeps them local-only. The release JSON takes its `enabled` and `history` (`False` turns it off too) but stays local-only unless this is a dict that says `"remote": True`. See [Private repositories](#private-repositories). |

## What it declares

The function does no HTTP itself; it declares targets in the calling package,
exactly as if the BUILD file had written them, and the engine builds them:

- `extract = False`: one `http_fetch` target named `name`.
- `extract = True`: an `http_fetch` named `<name>_archive` (its output is the
  archive, named `asset`), and an `archive.extract` named `name` whose `src`
  is it. `archive.extract` is the builtin `archive` plugin's driver
  ([ARCHIVE.md](ARCHIVE.md)), named by the function rather than shipped in the
  gh cdylib. So the gh plugin needs a heph that ships the builtin `archive`
  plugin; with an older one, an `extract = True` target fails with "unknown
  driver `archive.extract`".

The same holds for `credentials`: `<name>_release` sets `http_fetch`'s
`unpinned` option, so a private `release_asset` needs a heph whose `http_fetch`
knows it. An older one refuses the unknown key when it parses that target.
Likewise the asset fetch sets `http_fetch`'s `refresh` option: a newer gh
plugin on a host whose `http_fetch` predates it gets a loud unknown-key refusal
at parse, never a silently different target.

`<name>_archive` (and, with `credentials`, `<name>_release` and `<name>_url`)
are reserved in the calling package. Nothing refuses a hand-written target with
one of those names: a package holding two targets of one name answers with the
first, so one of them silently goes unused. Don't reuse them.

To see what a call declared:

```sh
heph query -e '//third_party/gh'                   # every target in the package
heph inspect spec //third_party/gh:gh_archive      # one target's driver and config
```

The URL is `{server_url}/{repo}/releases/download/{tag}/{asset}`. The tag is
percent-encoded per `/`-separated segment, keeping its `/`s (GitHub's own
download URLs do); the asset is one encoded segment. Braces are escaped, so
`http_fetch` does not read them as `{arg}` placeholders.

Fetching, verification and caching are `http_fetch`'s (see its driver docs).
The extracted tree's cache key comes from the archive's content through the
dep, plus `format`, `strip_components` and `out`.

## `out`

Without `extract`, `out` names the fetched file; with it, the directory the
archive unpacks into. Both are relative to the package:

```python
heph.gh.release_asset(name = "jq", repo = "jqlang/jq", tag = "jq-1.7.1",
                      asset = "jq-linux-amd64", executable = True)
# //p:jq is the file p/jq-linux-amd64; with out = "jq", the file p/jq

heph.gh.release_asset(name = "gh", repo = "cli/cli", tag = "v2.40.0",
                      asset = "gh_2.40.0_linux_amd64.tar.gz",
                      extract = True, strip_components = 1)
# //p:gh is the directory p/gh/ (bin/gh, share/…); with out = "tools/gh",
# the directory p/tools/gh/
```

With `extract`, `out` may not be `asset`: the archive is already that file.

## Pinning: `sha256`

`sha256` is the asset's — the archive's with `extract`, never the unpacked
tree's. To get it:

- Most releases publish a checksums asset (`checksums.txt`, `*.sha256`); copy
  the asset's line from it.
- Or download it once and hash it: `sha256sum gh_2.40.0_linux_amd64.tar.gz`.
- Or build without `sha256`: `http_fetch` warns that the fetch is unverified
  and prints the hash of what it fetched. Check it against the release before
  pasting it.

A wrong `sha256` fails the fetch with `checksum mismatch … expected …, got …`.

## Per-platform assets

A release ships one asset per platform, each with its own `sha256`. Declare one
`release_asset` per platform with `heph.core.matrix` (see `docs/MATRIX.md`).
Each cell names its own target and returns what `release_asset` returns,
`//<pkg>:<name>`:

```python
ASSETS = {
    "linux/amd64":  ("tool-1.2.0-x86_64-unknown-linux-gnu.tar.gz",  "<sha256>"),
    "linux/arm64":  ("tool-1.2.0-aarch64-unknown-linux-gnu.tar.gz", "<sha256>"),
    "darwin/arm64": ("tool-1.2.0-aarch64-apple-darwin.tar.gz",      "<sha256>"),
}

def _tool(platform):
    asset, sha = ASSETS[platform]
    return heph.gh.release_asset(
        name = "tool." + platform.replace("/", "-"),
        repo = "acme/tool",
        tag = "v1.2.0",
        asset = asset,
        sha256 = sha,
        extract = True,
        strip_components = 1,
    )

TOOL = heph.core.matrix(
    axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]},
    cell = _tool,
    default = {"platform": heph.core.os() + "/" + heph.core.arch()},
)
```

`TOOL.cells` is a list with one entry per platform, in axis order:
`[c.value for c in TOOL.cells]` is every platform's tool
(`//p:tool.linux-amd64`, ...), so a darwin laptop can bundle the `linux/arm64`
one, and `c.axes["platform"]` says which platform an entry is. Every cell is an
ordinary target. `TOOL.default` is this machine's tool
(`tools = [TOOL.default]`). On a machine with no cell the `heph.core.matrix()`
call fails, with a message listing the cells (leave `default` out to evaluate
the file anywhere). A consumer of `TOOL.default` has this machine's address in
its config, so its cache key moves with the machine.

The asset name is not templated: `http_fetch`'s `{goos}`-style placeholders
take one `sha256` for every platform, so they cannot pin a per-platform asset.

## Troubleshooting

- A 404 on the asset fetch: the `repo`, `tag` or `asset` does not exist
  (`heph inspect spec` shows the URL built from them), or the repository is
  private and needs `credentials`; see [below](#troubleshooting-private-assets).
- `checksum mismatch`: the asset changed, or `sha256` is another platform's.
- `nothing to extract: strip_components = N removed every entry`: the error
  lists the first entries; count their leading directories and lower
  `strip_components`.
- `raise max_bytes` / `raise max_entries`: the archive is larger than the
  default bounds; set the argument if it is meant to be.

## `server_url` and `api_url`

Arguments of each call. `server_url` defaults to `https://github.com`. Pass it
for a GitHub Enterprise server:

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

They are per call, not plugin options, because one workspace can pull from
more than one server. A workspace whose own code lives on a GitHub Enterprise
server still fetches a public tool from github.com by leaving them out:

```python
heph.gh.release_asset(name = "gh", repo = "cli/cli", tag = "v2.40.0",
                      asset = "gh_2.40.0_linux_amd64.tar.gz", extract = True)
```

`api_url` is where private assets are looked up and fetched. It defaults to
`https://api.github.com` when `server_url` is `https://github.com`, and to
`{server_url}/api/v3` for any other server, which is where GitHub Enterprise
serves its API. The default is derived from the same call's `server_url`.

Both are a scheme (`https://` or `http://`), a host and an optional path, with
no query, fragment or whitespace; trailing `/`s are dropped. A `user:password@`
in either is refused, since the URL lands in every declared target and log
line; a token goes in `credentials`. `http://github.com` is refused: github.com
is https only. Anything else fails the call naming the argument. Both URLs are
in the declared targets' config, so they are part of those targets' cache keys:
the same asset from two servers is two cache entries.

The plugin's only options are its Actions hook's, under `hook:`, and the hook
is built only with `hook: {enable: true}` (`docs/GHA_REPORTING.md` §12);
enabling the plugin for release assets needs no `options:` and loads no hook.

## Unpacking rules

With `extract = True` the archive is unpacked by `archive.extract`, in-process
and with the same code on every supported platform. The archive is untrusted
input; the rules it is held to (paths, links, modes, duplicates, bounds,
`strip_components`) are [ARCHIVE.md's](ARCHIVE.md#unpacking-rules). The call's
`format`, `strip_components`, `max_bytes`, `max_entries` and `out` are
forwarded to it as passed; the bounds are not hashed, and the extracted tree
is cached as a pure function of the archive's bytes.

## Private repositories

A private asset's download URL takes no token. GitHub serves it through the
API, by an asset id that is only known by asking the API. So with
`credentials` the function declares three targets instead of one fetch, and all
the network stays in `http_fetch`:

- `<name>_release`: an `http_fetch` of
  `{api_url}/repos/{repo}/releases/tags/{tag}` (the tag is one segment here,
  its `/`s as `%2F`), with the credentials and `unpinned = True`.
- `<name>_url`: a `gh.asset_url` that picks the asset named `asset` out of that
  JSON and writes its API URL. `gh.asset_url` is internal to this function, not
  a driver to write targets with; its config may change without notice. It refuses a URL outside `{api_url}/`, since the
  next fetch sends the credentials there. A name the release does not have
  fails here, and the error lists the names it does have.
- the asset (`name`, or `<name>_archive` with `extract`): an `http_fetch` with
  `url = "${read://<pkg>:<name>_url}"` (`docs/DEFERRED_VALUES.md`),
  `Accept: application/octet-stream`, the credentials, `sha256`, and
  `refresh = [":<name>_release"]`, so a 404 there names the command that
  refetches the JSON.

The release JSON is cached by its URL and headers, so a warm build makes no API
call. Its bytes change with every download (they carry download counts), so it
cannot take a `sha256`; `unpinned = True` says so, and `http_fetch` does not
warn about it. Pinning the asset is enough to build without a warning.

It is cached **locally only** by default. The JSON holds more than the asset it
leads to: the release notes, every asset's name, size and id, and the logins of
whoever uploaded them. Only `cache = {"remote": True}`, written as such, puts
it in the remote cache with the other targets, where anyone who can read that
cache can read it. Any other `cache` gives it its `enabled` and `history` and
keeps it local: `cache = True` or `{"history": 3}` sends the asset to the
remote cache but not the JSON. `cache = False` caches nothing, so every build
looks the release up again: one API call per private asset per build.

A cached JSON is a snapshot of the release when it was fetched, and its key is
the tag, not the content:

- An asset uploaded again under the same name gets a new id. The cached JSON
  still names the old one, so the asset fetch 404s, and its error names
  `//<pkg>:<name>_url`, the target the URL came from, and prints the command
  below.
- A JSON fetched after the release was created but before the asset was
  uploaded lacks the asset, so `<name>_url` fails with `the release has no
  asset named …`, and its error prints the command below.

Either way, refetch the JSON with `heph run --force //<pkg>:<name>_release`,
then build again. A forced run overwrites the cached entry, in the remote
cache too when it is stored there, from a machine that writes the remote
cache; a read-only client refreshes only its own.

The credential is a netrc presentation for the API host (`docs/CREDENTIALS.md`),
the token as the password. GitHub ignores the login, but netrc needs one:

```python
target(
    name    = "github",
    driver  = "auth.credential",
    # Set in Actions; give the job `permissions: contents: read`. Locally, a
    # personal access token that can read the repository.
    sources = [heph.auth.env(["GITHUB_TOKEN"])],
    present = heph.auth.netrc(["api.github.com"],
                              login = "x-access-token", password = "${github_token}"),
)

heph.gh.release_asset(
    name = "tool",
    repo = "acme/private-tool",
    tag = "v1.0",
    asset = "tool-linux-amd64",
    sha256 = "…",
    credentials = [":github"],
)
```

For GitHub Enterprise the `machine` is the server's host. The API answers the
asset fetch with a redirect to its storage host. The credential is not sent
there: `http_fetch` drops it on any redirect to another origin.

### Troubleshooting private assets

- 401, 403 or 404 on `<name>_release`: the token is missing, expired or cannot
  read the repository; the netrc `machine` is not the `api_url` host; or the
  `repo` or `tag` is misspelled (GitHub answers 404, not 403, for a private
  repository the token cannot see).
- `the release has no asset named …` on `<name>_url`: the error lists the
  names the release has. If the asset is on the release now, the cached JSON
  predates its upload: `heph run --force //<pkg>:<name>_release`.
- 401, 403 or 404 on the asset fetch (`name`, or `<name>_archive`): the token
  can read the release but not the asset. A `checksum mismatch` there is a
  wrong `sha256`, as for a public asset.
- 404 on the asset fetch after the asset was uploaded again: the cached
  release JSON names its old id. The error names `//<pkg>:<name>_url`, where
  the URL came from, and prints the exact command that fixes it:
  `heph run --force //<pkg>:<name>_release`, not the uncached `_url`.
- An identity without access to the repository gets a 404 on `<name>_release`
  only when the JSON is not cached. On a hit (a JSON fetched by another
  identity, from the same local cache or the remote one) it gets the JSON, then
  fails at the asset fetch, or reads the asset from the cache if it is there.
