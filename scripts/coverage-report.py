#!/usr/bin/env python3
"""Filter and summarise the lcov report grcov produces for `cov`.

Run by `cov` (see `devenv.nix`) on grcov's raw output. Three jobs, one parse:

**Strip `#[cfg(test)]` modules.** Source-based coverage instruments the test
modules along with the code they test, and in this tree that is 39% of all
Rust lines. Left in, they do not merely inflate the headline — they invert the
signal that matters. `.claude/testing.md` requires every change to ship with a
test, so *every* PR here is production-code-plus-test-module, and a PR adding
100 untested production lines beside 300 test lines scores ~75% patch coverage
and reads as well tested. Neither Codecov's `ignore:` nor grcov's `--ignore`
can reach these lines: both match on paths, and these live inside production
source files.

Why not `#[coverage(off)]`, which is what the ecosystem points at: it is still
unstable on this repo's toolchain (rustc 1.96 gives `error[E0658]: the
`#[coverage]` attribute is an experimental feature`; tracking issue #84605, open
since 2021). Adopting it would mean a nightly toolchain for the coverage legs,
`#![feature(coverage_attribute)]` in 42 crate roots, and an annotation on all
184 `#[cfg(test)] mod tests` blocks that a new module can silently forget —
which is the same failure this exists to close, moved somewhere with 184 chances
to happen. When it stabilises the attribute becomes the better answer, because
the regions are then excluded at instrumentation time rather than after the
fact, and this file can go.

The exclusion is computed from the source rather than by regex-matching line
markers, because the obvious grcov spelling (`--excl-start '^#\\[cfg\\(test\\)\\]$'`
with `--excl-stop '^\\}$'`) is wrong on this tree in a direction that fails
silently. 14 of the 198 column-0 `#[cfg(test)]` sites are not module blocks —
`#[cfg(test)] mod foo;` declarations, a `use`, two `const`s — and for those the
range would run to whatever the next column-0 `}` happened to be, quietly
*deleting production lines* from the denominator. Requiring the `mod … {` form
and finding its own closing brace cannot do that.

**Rewrite the lcov in place of the raw one**, keeping every record it did not
have to drop and recomputing the `LF`/`LH`/`FNF`/`FNH`/`BRF`/`BRH` counters, so
Codecov reads exactly what the table below reports.

**Report**, and refuse to publish a report that measured nothing. A worst-first
per-crate table on stdout, `summary.json` with sorted keys and no timestamps for
agents and for diffing two runs, and the floors — nothing downstream of `cov`
can fail, so the floors and the named canary are the last thing standing between
a broken collection and a plausible number that reads as a drop.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

CFG_TEST = "#[cfg(test)]"

# `mod <name> {` — the block form only, optionally `pub`/`pub(crate)`. A `mod
# foo;` declaration deliberately does not match: it opens no brace, so there is
# no range to find and guessing one deletes real code.
MOD_BLOCK = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{\s*$")


def cfg_test_ranges(source: str) -> list[tuple[int, int]]:
    """Inclusive 1-based line ranges of the `#[cfg(test)]` modules in `source`.

    Only column-0 `#[cfg(test)]` immediately followed by a column-0 `mod … {`,
    closed by the next column-0 `}`. Everything inside such a module is indented
    by rustfmt, so the first column-0 `}` after it is its own — this does not
    need to balance braces, and deliberately does not try.
    """
    lines = source.splitlines()
    ranges: list[tuple[int, int]] = []
    i = 0
    while i < len(lines):
        if lines[i].rstrip() == CFG_TEST and i + 1 < len(lines) and MOD_BLOCK.match(lines[i + 1]):
            end = i + 2
            while end < len(lines) and lines[end].rstrip() != "}":
                end += 1
            if end < len(lines):
                ranges.append((i + 1, end + 1))
                i = end
        i += 1
    return ranges


def excluded(ranges: list[tuple[int, int]], line: int) -> bool:
    return any(start <= line <= stop for start, stop in ranges)


class FileRecord:
    """One `SF:` … `end_of_record` block, filtered."""

    def __init__(self, path: str) -> None:
        self.path = path
        # line -> hits, accumulated: a file can appear in more than one record
        # (one per binary that linked it), and a line is covered if *any* of
        # them hit it. Overwriting would report whichever binary was parsed last.
        self.lines: dict[int, int] = {}
        self.kept: list[str] = []
        self.functions: dict[str, int] = {}
        self.branches: list[tuple[int, bool]] = []
        self.dropped = 0

    @property
    def hit(self) -> int:
        return sum(1 for n in self.lines.values() if n > 0)

    @property
    def total(self) -> int:
        return len(self.lines)


def parse(raw: str, source_root: Path, strip_cfg_test: bool) -> dict[str, FileRecord]:
    """Parse an lcov report, accumulating per file and dropping excluded lines."""
    records: dict[str, FileRecord] = {}
    ranges_cache: dict[str, list[tuple[int, int]]] = {}
    current: FileRecord | None = None
    ranges: list[tuple[int, int]] = []
    dropped_fns: set[str] = set()

    for raw_line in raw.splitlines():
        line = raw_line.rstrip("\r")
        if line.startswith("SF:"):
            path = line[3:].strip()
            current = records.setdefault(path, FileRecord(path))
            dropped_fns = set()
            if path not in ranges_cache:
                source = source_root / path
                try:
                    text = source.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    # grcov ran with --ignore-not-existing, so a path that
                    # cannot be read here is not one it reported lines for.
                    text = ""
                ranges_cache[path] = cfg_test_ranges(text) if strip_cfg_test else []
            ranges = ranges_cache[path]
            continue

        if current is None:
            continue

        if line.startswith("DA:"):
            number, _, rest = line[3:].partition(",")
            try:
                lineno = int(number)
                hits = int(rest.split(",", 1)[0])
            except ValueError:
                continue
            if excluded(ranges, lineno):
                current.dropped += 1
                continue
            current.lines[lineno] = current.lines.get(lineno, 0) + hits
            current.kept.append(f"DA:{lineno},{current.lines[lineno]}")
        elif line.startswith("FN:"):
            number, _, name = line[3:].partition(",")
            try:
                lineno = int(number)
            except ValueError:
                continue
            if excluded(ranges, lineno):
                # `FNDA:` is keyed by name, not by line, so the name has to be
                # remembered or its execution count outlives the function.
                dropped_fns.add(name)
                continue
            current.functions.setdefault(name, 0)
            current.kept.append(line)
        elif line.startswith("FNDA:"):
            count, _, name = line[5:].partition(",")
            if name in dropped_fns:
                continue
            try:
                current.functions[name] = current.functions.get(name, 0) + int(count)
            except ValueError:
                continue
        elif line.startswith("BRDA:"):
            number, _, rest = line[5:].partition(",")
            try:
                lineno = int(number)
            except ValueError:
                continue
            if excluded(ranges, lineno):
                continue
            taken = rest.rsplit(",", 1)[-1]
            current.branches.append((lineno, taken not in ("-", "0")))
            current.kept.append(line)
        elif line.startswith("end_of_record"):
            current = None

    return records


def write_lcov(records: dict[str, FileRecord], out: Path) -> None:
    """Emit the filtered report, with every counter recomputed.

    Recomputed rather than carried over: a stale `LF:`/`LH:` next to a filtered
    `DA:` list is a report that disagrees with itself, and consumers differ on
    which half they believe.
    """
    chunks: list[str] = []
    for path in sorted(records):
        record = records[path]
        if not record.lines:
            continue
        body = [f"SF:{path}"]
        for name, count in sorted(record.functions.items()):
            body.append(f"FNDA:{count},{name}")
        body.extend(line for line in record.kept if line.startswith(("FN:", "BRDA:")))
        body.append(f"FNF:{len(record.functions)}")
        body.append(f"FNH:{sum(1 for n in record.functions.values() if n > 0)}")
        for lineno in sorted(record.lines):
            body.append(f"DA:{lineno},{record.lines[lineno]}")
        body.append(f"LF:{record.total}")
        body.append(f"LH:{record.hit}")
        if record.branches:
            body.append(f"BRF:{len(record.branches)}")
            body.append(f"BRH:{sum(1 for _, taken in record.branches if taken)}")
        body.append("end_of_record")
        chunks.append("\n".join(body))
    out.write_text("\n".join(chunks) + "\n", encoding="utf-8")


def unit_of(path: str) -> str:
    """The reporting unit a source path belongs to.

    Crates are the granularity a human acts on: "coverage dropped" is not
    actionable, "coverage dropped in crates/engine" is.
    """
    parts = [part for part in path.split("/") if part not in ("", ".")]
    if not parts:
        return "<unknown>"
    if path.startswith("/"):
        # grcov's `--ignore '/*'` should have removed these; name them rather
        # than rendering a blank row if one ever survives.
        return "<external>"
    if parts[0] == "crates" and len(parts) > 1:
        return f"crates/{parts[1]}"
    if parts[0] == "src":
        # The root `heph` package's own sources.
        return "heph"
    return parts[0]


def pct(hit: int, total: int) -> float:
    return round(100.0 * hit / total, 2) if total else 0.0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("lcov", type=Path, help="grcov's raw lcov report")
    ap.add_argument(
        "--source-root",
        type=Path,
        default=Path("."),
        help="root the lcov paths are relative to",
    )
    ap.add_argument("--out-lcov", type=Path, help="write the filtered report here")
    ap.add_argument("--json", type=Path, help="write a summary.json here")
    ap.add_argument(
        "--strip-cfg-test",
        action="store_true",
        help="drop lines inside column-0 `#[cfg(test)] mod … {}` blocks",
    )
    ap.add_argument(
        "--label",
        default="",
        help="platform label printed with the totals (e.g. linux/amd64)",
    )
    # The three below are the floors that make an empty report fail rather than
    # publish. Nothing downstream of `cov` can: Codecov is configured
    # informational and the CI job gates nothing, so a report that measured
    # almost nothing would land as a plausible number and read as a drop.
    ap.add_argument(
        "--min-files",
        type=int,
        default=0,
        help="fail if fewer than this many source files appear in the report",
    )
    ap.add_argument(
        "--min-lines",
        type=int,
        default=0,
        help="fail if fewer than this many instrumented lines appear",
    )
    ap.add_argument(
        "--require-covered",
        action="append",
        default=[],
        metavar="PATH",
        help=(
            "fail unless this source path is present with at least one hit. A "
            "size floor can be met by build-script noise; a named file that "
            "must have been executed cannot."
        ),
    )
    args = ap.parse_args()

    raw = args.lcov.read_text(encoding="utf-8", errors="replace")
    records = parse(raw, args.source_root, args.strip_cfg_test)
    records = {path: record for path, record in records.items() if record.lines}

    if args.out_lcov:
        write_lcov(records, args.out_lcov)

    if not records:
        print(
            f"error: {args.lcov} names no source files with instrumented lines "
            f"— the report is empty, which is not the same thing as 0% coverage",
            file=sys.stderr,
        )
        return 1

    per_unit: dict[str, list[int]] = {}
    for path, record in records.items():
        acc = per_unit.setdefault(unit_of(path), [0, 0])
        acc[0] += record.hit
        acc[1] += record.total

    grand_hit = sum(record.hit for record in records.values())
    grand_total = sum(record.total for record in records.values())
    stripped = sum(record.dropped for record in records.values())

    # Worst first, then by name so equal percentages have a stable order (a
    # run-to-run reshuffle in a CI log reads as a change that isn't one).
    units = sorted(per_unit.items(), key=lambda kv: (pct(*kv[1]), kv[0]))

    for name, (hit, total) in units:
        print(f"{pct(hit, total):6.2f}%  {hit:7d}/{total:<7d}  {name}")
    label = f" [{args.label}]" if args.label else ""
    print(f"{pct(grand_hit, grand_total):6.2f}%  {grand_hit:7d}/{grand_total:<7d}  TOTAL{label}")
    if args.strip_cfg_test:
        print(f"({stripped} lines in #[cfg(test)] modules excluded)")

    if args.json:
        payload = {
            "label": args.label,
            "stripped_cfg_test_lines": stripped,
            "totals": {
                "line_coverage": pct(grand_hit, grand_total),
                "lines_hit": grand_hit,
                "lines_total": grand_total,
            },
            "units": [
                {
                    "line_coverage": pct(hit, total),
                    "lines_hit": hit,
                    "lines_total": total,
                    "name": name,
                }
                for name, (hit, total) in units
            ],
            "files": [
                {
                    "line_coverage": pct(record.hit, record.total),
                    "lines_hit": record.hit,
                    "lines_total": record.total,
                    "path": path,
                }
                for path, record in sorted(records.items())
            ],
        }
        args.json.write_text(
            json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    # Checked after the table is printed, so a failure still shows what *was*
    # measured — the first thing needed to tell "collection broke" from "a crate
    # stopped being built".
    problems = []
    if len(records) < args.min_files:
        problems.append(f"only {len(records)} source files in the report (floor: {args.min_files})")
    if grand_total < args.min_lines:
        problems.append(
            f"only {grand_total} instrumented lines in the report (floor: {args.min_lines})"
        )
    for required in args.require_covered:
        record = records.get(required)
        if record is None:
            problems.append(f"{required} is absent from the report entirely")
        elif record.hit == 0:
            problems.append(f"{required} has 0 of {record.total} lines covered")

    if problems:
        print(file=sys.stderr)
        print(
            "error: this report is too small to be a coverage measurement — "
            "collection failed, it is not a drop:",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
