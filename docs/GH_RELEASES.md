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
| `cache` | local and remote | Caching for the asset (fetched, and unpacked with `extract`) and, with `credentials`, the release JSON. The shape every driver's `cache` takes: `False` turns off both tiers, `{"remote": False}` keeps them local-only. |

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

A release ships one asset per platform, each with its own `sha256`. Pick by the
host's `heph.core.os()` (`linux`, `darwin`) and `heph.core.arch()` (`amd64`,
`arm64`):

```python
ASSETS = {
    "linux/amd64":  ("tool-1.2.0-x86_64-unknown-linux-gnu.tar.gz",  "<sha256>"),
    "linux/arm64":  ("tool-1.2.0-aarch64-unknown-linux-gnu.tar.gz", "<sha256>"),
    "darwin/arm64": ("tool-1.2.0-aarch64-apple-darwin.tar.gz",      "<sha256>"),
}
asset, sha = ASSETS[heph.core.os() + "/" + heph.core.arch()]

heph.gh.release_asset(
    name = "tool",
    repo = "acme/tool",
    tag = "v1.2.0",
    asset = asset,
    sha256 = sha,
    extract = True,
    strip_components = 1,
)
```

Consumers name `//p:tool` on every host. A host missing from the dict fails the
BUILD file's evaluation on the dict lookup, naming the key. The asset name is not templated:
`http_fetch`'s `{goos}`-style placeholders take one `sha256` for every
platform, so they cannot pin a per-platform asset.

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
  `Accept: application/octet-stream`, the credentials and `sha256`.

The release JSON is cached like any fetch, by its URL and headers, locally and
remotely, so a warm build makes no API call. Its bytes change with every
download (they carry download counts), so it cannot take a `sha256`;
`unpinned = True` says so, and `http_fetch` does not warn about it. Pinning the
asset is enough to build without a warning.

A cached JSON keeps naming the asset id it was fetched with. An asset uploaded
again under the same name and tag gets a new id, and fetching the old one fails
with a 404 whose hint says to clean what the URL was read from:
`heph tool clean //<pkg>:<name>_release`, then run again. Remote caching
exposes nothing new: anyone who can read the remote cache can already read the
pinned asset it leads to (`docs/CREDENTIALS.md`).

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
  names the release has.
- 401, 403 or 404 on the asset fetch (`name`, or `<name>_archive`): the token
  can read the release but not the asset. A `checksum mismatch` there is a
  wrong `sha256`, as for a public asset.
- 404 on the asset fetch after the asset was uploaded again: the cached
  release JSON names its old id. `heph tool clean //<pkg>:<name>_release`.
