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

`go list`, as heph invokes it, reads only part of most files: of a `.go` file,
the header and, if the file imports `"embed"`, its `//go:embed` lines; of every
other file, at most a name and a leading comment block. `_golist`'s key follows
that, so editing a function body (in a `.go` or `_test.go` file) or a non-Go
file's bytes (an embedded asset, a README, a `testdata` file, the body of a `.c`)
doesn't re-run it. Adding, removing or renaming a file still does.

| I changed… | `_golist` re-runs? |
|---|---|
| a function body, in a `.go` or `_test.go` file | no |
| an import | yes |
| a `//go:build` line (in a `.go` file, or in a `.c`/`.s`/… header) | yes |
| a `//go:embed` line, moved to another line | yes |
| a byte of an embedded asset | no |
| a new file, or a removed or renamed one | yes |
| a generated `.go` file's body | no |
| a generated `.go` file's imports | yes |
| a generated non-Go file's content | yes |

"As heph invokes it" matters: heph runs `go list -e` with a fixed set of `-json`
fields, none of which needs the code below the imports, and adds `-test` for a
package that has `_test.go` files. With `-test`, a
syntax error in a test file's body is reported as an `Error` on the synthetic
`pkg.test` entry, and `_golist` keeps only the package's own entry and drops that
one, so `_golist`'s result is the same with or without the error. The compile of
the test binary reports it.

When `_golist` does re-run and lists the same package, nothing below it rebuilds
for that reason: a target that takes `_golist` as an input is keyed on the hash
of its output, not on whether it ran, and the targets heph derives from the
listing are defined from the output itself. The same cutoff is what keeps
`_golist` a hit when the header target re-runs on a body edit.

This holds for the Go versions the rules were checked against (Go 1.27.3) and is
applied to every Go version, with no version gate; it is re-checked against Go's
source on each toolchain bump.

The package's own **`.go` files**, `_test.go` included, are staged whole, and the
key covers each file's name and what `go/build` reads of it, through the same
`//@heph/go/headers/<pkg>:headers` target as the header-read files below. That is
the header — leading comments, build constraints and `//go:` directives, the
package clause, the imports with their doc comments (the cgo preamble) — plus,
for a file that imports `"embed"`, every `//go:embed` and `//line` comment and
where it sits. Changing an import, a constraint, the preamble or an embed pattern
re-runs `_golist`. Editing code below the imports doesn't, except in a file that
imports `"embed"`, where an edit that moves a `//go:embed` line to another line
does (`go list` reports that position when a pattern matches nothing).

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

**Generated files** (the outputs of a `go_src`-labelled target, or of a target
named in `go_codegen_deps`) are staged whole and keyed through the same header
target, by the target that produced them and:

- for a generated `.go` file, its go/build view, as for a checked-in one. A
  generator whose output changes only below the imports re-runs itself and the
  compile, not `_golist`; one that changes an import, a constraint or an embed
  pattern, or writes a new file, re-runs `_golist`.
- for any other generated file, its whole content. Changing a generated asset
  re-runs `_golist`.

A path that both a checked-in file and a generator produce, or that two
generators produce, fails the header target instead of keying only one of them.

**A vendored module** (one with `vendor/modules.txt`) keeps every file, `.go`
and generated files included, content-hashed, because `go list` reads
`vendor/modules.txt`.

### Seeing what `_golist` is keyed on

The header target's output is a tar with one entry per file, holding what the key
covers of it. `heph inspect deps` prints its exact addr (it carries arguments
when the package has `go_codegen_root` or `go_codegen_deps`):

```bash
heph inspect deps '//lib:_golist@v=host' | grep headers
heph r --cat-out '//@heph/go/headers/lib:headers' > before.tar
tar -tvf before.tar               # the files that reach the key
tar -xOf before.tar lib/lib.go    # what of lib.go reaches it
```

Each `.go` entry starts with `H` (the header), `W` (the whole file, when its
header doesn't parse) or `E` (the header plus the `//go:embed` and `//line`
records); a generated file's entry starts with the addr of its producer, then a
`V` and the view (`.go`) or a `C` and a content hash (anything else). A
`gen_deps=` argument is the hex of the `go_codegen_deps` list:
`echo <hex> | xxd -r -p`.

To compare two runs, save the tar on each and `diff <(tar -xOf before.tar)
<(tar -xOf after.tar)`, or compare `heph inspect hashout` of the header target
and `heph inspect hashin '//lib:_golist@v=host'` across them: an unchanged
`hashin` means `_golist` is a cache hit.

If `go list`'s result looks stale — a file, an import or an embed pattern that
`_golist` should report and doesn't — that is a bug in this keying: report it,
with the two `tar -xOf` outputs (or the two `hashin`s) as the evidence.

*Internal:* the names lane is an fs glob with `m=names`. You can name one in a
BUILD file by its raw addr, but it's only valid for a consumer that reads nothing
but names: the placeholders are empty and their bytes aren't part of any key.
