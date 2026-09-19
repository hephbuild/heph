# Credentials

Run it:

```bash
heph auth status                                # which identities apply here
heph auth explain //credential:demo             # and why the chain picked what it picked
heph run //credential:uses-it
heph run //credential:leaks-it --no-tui         # the token comes out as [redacted]
```

Nothing here needs setup or a real secret: the sources produce fake material from
`printf`.

## What to read for

**The consumer says one word.** `credentials = [demo]` is the entire integration
— no branching on CI, no wrapper script, no environment names, and the same file
on a laptop and a runner. Everything environment-shaped lives in the declaration
above it, written once.

**The chain is ordered, and the environment picks the winner.** Run
`heph auth explain //credential:demo` and read the walk: the OIDC source is
skipped here because there is no OIDC endpoint on a laptop, so the `exec` source
wins. On a GitHub Actions runner the same two lines resolve the other way. The
rule to internalise is that a probe answers *"is this applicable here?"*, not
*"will it succeed?"* — the first applicable source **is** the source, and an
acquire failure is terminal rather than a fallthrough. If a chain fell through on
failure, a misconfigured role in CI would silently fall back to whatever ambient
identity the runner happened to have.

**The cache key never learns there was a credential.** Add `credentials = [demo]`
to a target and its `heph inspect hashin` does not move — not for the reference,
not for the declaration, not for the names of the variables it presents. That is
structural: the reference is an input with `hashed: false, runtime: false`, so it
has no path into `hashin` at all. It is also what makes the contract enforceable
rather than aspirational:

> A target's outputs must be identical whichever identity satisfied its
> credential requirement.

**A presentation has nowhere to put configuration.** Three keys — `env`, `files`,
`helper` — and nothing else. A region, an account id, a project, a profile: every
one of those *selects bytes*, so it is an ordinary hashed input on the consumer.
Try adding `"region": "eu-west-1"` to a `present` block and heph will tell you
where it belongs.

**A leaked token is scrubbed before it reaches disk.** `//credential:leaks-it`
echoes its own credential. The scrubbing happens at the output tee, upstream of
`log.txt` — which matters because that log is packed into the cache as an
artifact and lifted into the failure event, so redacting at render time would be
too late.

## What is deliberately absent

- **No `heph.auth.vault()`, no `heph.auth.op()`, no `heph.auth.aws_sso()`.** That
  list has no end, and each entry would be a workflow frozen into a plugin for no
  gain over four lines. A source is `exec` with a field map, or it is a target.
  The *presentation* presets exist for the opposite reason: those are third-party
  wire formats with exactly one correct encoding.
- **No command that prints material to a terminal.** It would become a shell
  history entry, a screen share and a paste into a chat window. The two honest
  uses it would serve are already served by `heph shell` and `heph auth explain`.
- **No `--no-credentials`.** Withholding an identity produces a permission error,
  not an audit. The real audit is two identities and one output hash.

See `docs/CREDENTIALS.md` for the full model, the helper dialects, and the
security boundary.
