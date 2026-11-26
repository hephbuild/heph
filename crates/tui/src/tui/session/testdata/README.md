# Session recording fixtures

`format-N/session.db` is a small sealed recording in session format `N`
(`store::FORMAT`), committed so that `store_tests::opens_format_1_fixture`
proves every released format still opens and answers the recipes in
`docs/SESSIONS.md` (I5).

A released format's fixture is frozen: never regenerate it. On a format bump,
write the new format's fixture with the generator, which refuses to overwrite
one that exists:

```sh
cargo test -p tui --lib generate_format_fixture -- --ignored
```

It writes `format-<store::FORMAT>/session.db` from `store_tests::write_fixture`.
