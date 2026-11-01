# Fetching Go modules

heph fetches a third-party Go module with `go` itself, in two kinds of target:
`go.download` runs `go mod download`, and `_golist` runs `go list`, which fetches
what it has to read. Both use your normal Go and git setup, and neither needs any
heph configuration.

## Private modules

Set the variables you already use for `go get`, in the environment heph runs in:

```bash
export GOPRIVATE=github.com/acme/*
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

## Under a runner

A target with a `runner` doesn't get heph's `PATH`, because the runner's own
environment applies. So `git`, `ssh` and your config have to exist inside that
environment, and `HOME` has to be somewhere they can be read. A variable a runner
strips has to go in its `runtime_pass_env`. See [Exec runners](EXEC_RUNNERS.md).

## What heph passes to `go`

Only these host variables reach a fetch:

| Purpose | Variables |
|---|---|
| Go | `GOPATH`, `GOPROXY`, `GONOPROXY`, `GOSUMDB`, `GONOSUMDB`, `GOPRIVATE`, `GOINSECURE`, `GOVCS`, `GOAUTH` |
| Config and credentials | `HOME`, `NETRC`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `GIT_ASKPASS`, `GIT_SSH`, `GIT_SSH_COMMAND`, `SSH_AUTH_SOCK` |
| Network | `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NIX_SSL_CERT_FILE`, `GIT_SSL_CAINFO`, `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, and their lowercase forms |

`PATH` is also passed, except under a runner and when listing the standard library.

`GOFLAGS` is not passed, because it can change what `go list` reports and it isn't
part of any cache key.

## How the cache treats a private module

None of these variables is part of a cache key. The key names the module and
version, not their content. A public module is checked against the Go checksum
database when it's fetched, but `GOPRIVATE` turns that check off. So if a private
tag moves, heph keeps serving the bytes it already cached for that version.

A failed fetch fails the target and isn't cached, so a fixed credential takes effect
on the next run.
