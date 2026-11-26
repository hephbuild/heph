# Session recording fixtures

`format-N/session.db` is a small sealed recording in session format `N`
(`store::FORMAT`), committed so that `store_tests::opens_format_1_fixture`
proves every released format still opens and answers the recipes in
`docs/SESSIONS.md` (I5).

`format-N/example.hephsession` is a real export in format `N`, as
`HEPH_SESSION_EXPORT_DIR` writes it at seal: a failed `heph run` whose root
failure's log is in `failure_logs`, with one approval whose notice text is in
`notices`, not a snapshot. `export_tests::opens_export_fixtures` proves every
released format's export still opens through the viewers' path and reads its
state, notice, failure log and tables from itself alone.

A released format's fixtures are frozen: never regenerate them. On a format
bump, write the new format's fixtures with the generators, which refuse to
overwrite one that exists:

```sh
cargo test -p tui --lib generate_format_fixture -- --ignored
cargo test -p tui --lib generate_export_fixture -- --ignored
```

`derivation-N/session.db` is a sealed recording whose explorer rows were
derived by derivation `N` (`store::DERIVATION`), committed so that
`store_tests::older_derivation_keeps_machine_rows` proves an older heph
reads PR 5's machine rows as they are instead of re-deriving them away. It
is frozen too; `generate_derivation_fixture` writes one for a new
derivation that needs it.

They write `format-<store::FORMAT>/session.db` from
`store_tests::write_fixture`, and `format-<store::FORMAT>/example.hephsession`
from `export_tests::write_export_fixture`.
