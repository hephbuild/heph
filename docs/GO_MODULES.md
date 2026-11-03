# Fetching Go modules

heph fetches a third-party Go module with `go` itself, in two kinds of target:
`go.download` runs `go mod download`, and `_golist` runs `go list`, which fetches
what it has to read. Both use your normal Go and git setup, and neither needs any
heph configuration.

## Private modules

Set `GOPRIVATE` the way you already do for `go get`. Either of these works:

```bash
export GOPRIVATE=github.com/acme/*
go env -w GOPRIVATE=github.com/acme/*
```

`go` then skips the proxy and the checksum database for those modules and runs
`git` directly. This works with every `gotool`, including a pinned hermetic SDK.

`git` needs the same things it needs outside heph:

- **`git` itself, on `PATH`**, plus `ssh` and any credential helper your git config
  names. heph downloads the Go SDK, but it doesn't provide git.
- **Your git config**, read from `HOME` or `XDG_CONFIG_HOME`, or from
  `GIT_CONFIG_GLOBAL` if it's set. That covers `url.insteadOf` and
  `credential.helper`.
- **Your credentials**, from one of `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`
  or a credential helper. `go` turns off git's terminal prompts, so a missing
  credential fails rather than hangs.

If a fetch fails, the target fails and names the package. Run `go list <package>` in
the same shell to reproduce it. A failure isn't cached, so a fixed credential takes
effect on the next run.

Every command that resolves targets needs access, including `heph query`. A build
that imports a private module you can't reach fails rather than leaving the import
out.

## Why a hermetic fetch sees the host's `PATH`

Targets normally don't get the host's directories on `PATH`
([Builtin coreutils](COREUTILS.md)). A module fetch is the exception, because `go`
has to find `git`, and `git` has to find `ssh` and your credential helpers. All of
those vary by machine, so declaring them would mean no setup could be shared.

`go.download` always gets it, and `_golist` gets it whenever it reads modules. It
doesn't apply in two cases:

- **Under a runner**, where the runner's own environment applies.
- **A hermetic listing of the standard library**, which fetches nothing.

`go list` runs no compiler here, so `PATH` can't change what it reports. All it can
change is whether a fetch succeeds, and a failed fetch fails the target.

## Under a runner

The variables below still reach `go` under a runner, but `PATH` doesn't. `git`, `ssh`
and your config have to exist in the runner's environment, and paths such as `HOME`
and `SSH_AUTH_SOCK` have to mean something there. See [Exec runners](EXEC_RUNNERS.md).

## What heph passes to `go`

Only these host variables reach a fetch:

| Purpose | Variables |
|---|---|
| Go | `GOPATH`, `GOPROXY`, `GONOPROXY`, `GOSUMDB`, `GONOSUMDB`, `GOPRIVATE`, `GOINSECURE`, `GOVCS`, `GOAUTH` |
| Config and credentials | `HOME`, `NETRC`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `GIT_ASKPASS`, `GIT_SSH`, `GIT_SSH_COMMAND`, `SSH_AUTH_SOCK` |
| Network | `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NIX_SSL_CERT_FILE`, `GIT_SSL_CAINFO`, `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, and their lowercase forms |

From your `go env -w` file, heph takes only the Go variables above that say where
modules come from, and an exported variable wins over the file. `go` itself runs
with `GOENV=off`.

`GOFLAGS` is not passed, from the environment or that file, because it can change
what `go list` reports and it isn't part of any cache key. Set build tags in heph
instead of in `GOFLAGS`.

## How the cache treats a private module

None of these variables is part of a cache key. The key names the module and
version, not their content. A public module is checked against the Go checksum
database when it's fetched, but `GOPRIVATE` turns that check off for `go.download`.
That has two consequences:

- **A moved tag keeps serving the old bytes.** heph already has that version cached,
  so publish a new version rather than moving a tag.
- **Git config can change the bytes.** A repository using Git LFS, for example, gives
  real files on a machine with the LFS filter configured and pointer files on one
  without, under the same key. Machines sharing a remote cache need the same setup.

## Cost

A private module is fetched twice on a cold build: `go list` fetches it into your own
module cache, and `go.download` fetches it again into heph's. Warm builds fetch
nothing.

## What re-runs `_golist`

`go list` reads a package's `.go` files whole, but of every other file it reads at
most a name and a leading comment block. `_golist`'s key follows that, so editing a
non-Go file's bytes (an embedded asset, a README, a `testdata` file, the body of a
`.c`) no longer re-runs it. Adding, removing or renaming a non-Go file still does.

Non-Go files reach `_golist` in one of two lanes:

- **Header-read files** are the package's own-directory files ending in `.c .cc .cxx
  .cpp .m .h .hh .hpp .hxx .f .F .for .f90 .s .S .sx .swig .swigcxx` whose name
  doesn't start with `_` or `.`. `go list` reads their build constraints, so they're
  staged whole, and the key covers each file's name and header (everything up to
  the first line of code) through `//@heph/go/headers/<pkg>:headers`. That target
  is cached locally only. Editing a `//go:build` line re-runs `_golist`, and
  editing the code below it doesn't.
- **Everything else** in the package's subtree is staged as an empty placeholder
  and keyed on its path alone.

Two cases keep hashing content:

- **A vendored module** (one with `vendor/modules.txt`) keeps every non-Go file
  content-hashed, because `go list` reads `vendor/modules.txt`.
- **The codegen lanes** (files produced by another target) stay content-hashed.

The names lane is an fs glob with `m=names`. You can name one in a BUILD file by
its raw addr, but it's only valid for a consumer that reads nothing but names: the
placeholders are empty and their bytes aren't part of any key.
