# Review brief formats

Every file the brief program reads or writes is JSON with a `format` string that names the
format and its version. A file with the wrong `format` is refused. Unknown fields are refused too:
the program decodes with `DisallowUnknownFields`, so a typo in a field name fails render instead
of being dropped.

**A fix-round agent reads the page data below, not the page.** The page draws these files; it
holds nothing they do not.

The source of truth is the Go types in `brief/`: the inputs in `triage.go`, the page data at the
top of `render.go`, the index in `index.go`, the pin in `pin.go`. Change a type and this file in
the same commit, and bump the version in the `format` string when an old file would no longer
read the same.

## Shared shapes

**Layer number.** Everywhere a field is called `layer` or `pr`, it is the layer's **PR number**,
not its position in the stack.

**Tier**: `"scrutinize"`, `"glance"` or `"ai-cleared"`. Counted is not a tier: the program assigns
it from a line's class, and no input can move a line into or out of it.

**Reason**: one slug from the closed list (`Reasons` in `triage.go`): `cache-key`,
`stored-format`, `concurrency`, `engine-flow`, `compatibility`, `security`, `hermeticity`,
`unsafe-seam`, `platform`, `decision`, `low-confidence`, `weakened-test`, `stack-design`. Any
other value is refused.

**RangeRef**, a `file:line-range` on one side of a layer's diff:

| Field | Type | Meaning |
| --- | --- | --- |
| `path` | string | Repository-relative path |
| `side` | `"new"` or `"old"` | Omitted means `new`. `old` is for removed lines, numbered at the layer's base |
| `start`, `end` | int | Inclusive, 1-based. `end` may not run past the file on that side |
| `note` | string, optional | What the range holds, shown when a card links instead of embedding |

Render refuses a range that is empty or inverted, names a file the layer does not change on that
side, runs past the file's end, or holds none of the layer's changed lines.

**LayerRef**: a RangeRef with a `layer` field beside its own fields (flattened, not nested).

**Read**, the ranges an agent reports it read:

| Field | Type | Meaning |
| --- | --- | --- |
| `agent` | string | `decider` or `stack-review` for those two; the ledger's "read by the decider or the stack reviewer" count matches these names exactly |
| `layer` | int | The layer, or `0` for any layer |
| `ranges` | RangeRef[] | What it read |

## The four inputs

### triage.json: `heph-review-brief-triage/2`

The decider's answer. Version 2 added the card text a reviewer who has not read the code needs:
`title`, `context`, `options`, `examples`, `diagram` and `used_by`. A version 1 file is refused.

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | `heph-review-brief-triage/2` |
| `items` | Item[] | Every card and group |
| `not_in_code` | NotInCode[], optional | Points with no code to show (I12). Not cards |
| `reads` | Read[] | What the decider read |

**Item**, one card or group:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Unique in the stack. Required |
| `layer` | int | The PR number the item belongs to |
| `tier` | Tier | Required |
| `reason` | Reason | Required at Scrutinize; optional below |
| `title` | string | What changed, in plain words: the card's heading. At most 90 characters. Required at Scrutinize; below it the page falls back to the claim |
| `context` | string | What the code is for, what reaches it (callers, the command or stage) and how it shows up to whoever meets it (a heph user, a BUILD author, a plugin author). At most 600 characters. Required at Scrutinize |
| `claim` | string | One sentence on what the code now does differently: shown as "Why it's flagged". Required |
| `question` | string | Required at Scrutinize: one decision, answerable with yes, no or a choice. At most 300 characters |
| `options` | Option[] | 2 to 4 answers to the question, each with what follows from it. Required at Scrutinize |
| `examples` | Example[], optional | Up to 3 concrete pictures of how the change shows up |
| `diagram` | string, optional | Mermaid source, at most 40 lines, from the subset below |
| `ranges` | RangeRef[] | At least one (I12). The lines that carry the decision, not a whole function |
| `used_by` | RangeRef[], optional | Up to 3 sites that use the changed code, each with a `note`. Read at the layer's head; they need not be in the diff |
| `start_from` | string[], optional | For a whole new file: the functions to read first |
| `tests` | TestRef[], optional | Tests that pin it. Each must exist at the layer's head and not be `#[ignore]`d |
| `board` | BoardNote[], optional | What each board agent said about this code |
| `weakenings` | string[], optional | Ids of weakenings from the index this card answers. Only on a Scrutinize card counts |
| `surfaced` | string, optional | The `id` of the surface item this card carries |
| `proposed_by` | string | Who proposed it: a board agent's name, `scout:<agent>`, `decider` |
| `tiered_by` | string, optional | Defaults to `decider` |
| `raised_by` | string, optional | Set by render when the stack reviewer promotes the item. Leave it empty |

**Option**: `choice` (at most 80 characters) and `impact` (at most 300): what follows if the
reviewer picks that choice, who is affected, and what changes, breaks or stays deferred.

**Example**: `label` (at most 40 characters, such as "Before", "After", "What a user sees") and
`text` (plain text, at most 30 lines, shown preformatted).

**The card-text rules**, each a render refusal:

- Over a cap above.
- A bare spec or decision id in `title`, `question` or an option: a token matching
  `\b[A-Z][0-9]{1,3}\b` (U28, P20, D4, I11), except the product names S3, R2, V8, L1, L2 and
  L3. The reviewer decides from those fields, so they say what the rule says. `context` may name
  an id beside its meaning.
- A `diagram` whose first keyword (after `%%` comments) is not `flowchart`, `graph`,
  `sequenceDiagram`, `stateDiagram` or `stateDiagram-v2`; or that has a `%%{` directive, a
  `click`, `link` or `links` line, a `<`, or `javascript:`.
- A `used_by` range on the old side, without a `note`, empty or inverted, over 40 lines, or not
  inside the file at the layer's head (each layer's own head, not the top's).

**TestRef**: `name` (the test function's name), `path` (optional, narrows the lookup to one file).

**BoardNote**: `agent`, `said`, and `status`, one of `fixed`, `overruled`, `open`. An `open` or
`overruled` note puts the card in "Decisions waiting for you".

**NotInCode**: `text` and `link` (where the point was raised), both required.

A live line's tier is the highest tier of any item covering it, so a line counts once.

### audit.json: `heph-review-brief-audit/1`

The stack reviewer's answer. It can only promote (I4).

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | `heph-review-brief-audit/1` |
| `assessment` | Assessment | One paragraph on the stack, top of the page. Required |
| `promotions` | Promotion[], optional | Items it raises |
| `items` | Item[], optional | New items, reason `stack-design` or `decision`. `proposed_by` and `tiered_by` default to `stack-review` |
| `confirmations` | Confirmation[], optional | Per layer, its approval of the AI-cleared tier |
| `reads` | Read[] | What it read, with `agent: "stack-review"` |

**Assessment**: `text`, and `refs`, a LayerRef[] with at least one entry. Each statement in the
text cites code (I12).

**Promotion**: `item` (an id in the triage), `tier` (at least the item's current tier; lower is
refused), `note` (why, shown as "raised by"), and `reason`, `title`, `context`, `question` and
`options`, which fill the card's own when it had none. A promotion to Scrutinize supplies each one
the item lacks; the card is then checked like any other.

**Confirmation**: `layer` and `note`. The page then says "AI-approved: N lines, by triage and
stack review" for that layer.

### surface.json: `heph-review-brief-surface/1`

What the board asked the user to see, written by `/board review` from the final round on the
pushed SHA.

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | `heph-review-brief-surface/1` |
| `items` | SurfaceItem[] | Every item every agent surfaced |
| `vouched` | Read[], optional | Ranges each agent read and vouches for, `agent` set to its name |

**SurfaceItem**:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Unique in the file, e.g. `hermeticity-1` |
| `agent` | string | The board agent |
| `layer` | int | The PR number |
| `tier` | Tier | The agent's own call. The carrying card may not go below it |
| `reason` | Reason | |
| `claim` | string | |
| `question` | string, optional | |
| `status` | string, optional | `fixed`, `overruled` or `open`, when it was a finding |
| `ranges` | RangeRef[] | The code it is about |

Every surface item must be carried by a triage item whose `surfaced` is its `id`, at no lower
tier.

### scout.json: `heph-review-brief-scout/1`

One scout's answer for one layer, or for its part of a split layer.

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | `heph-review-brief-scout/1` |
| `agent` | string | A name unique among the scouts, e.g. `scout-694-engine` |
| `layer` | int | The PR number |
| `candidates` | ScoutCandidate[] | What it proposes the decider look at |
| `reads` | RangeRef[] | What it read. The reads of every scout on a layer together must cover all its live lines |

**ScoutCandidate**: `claim`, `reason` (optional), `tier` (its own call), `why` (optional, one
line), `ranges`, and the optional proposals `context`, `examples`, `diagram` and `used_by`, in
the Item's shapes. Render does not check a scout's proposals; it checks what the decider copies
into the triage.

## Program-written intermediates

These are written by `brief pin` and `brief index` and read back by the program. Agents read
them; nobody writes them by hand.

- **pin.json**, `heph-review-brief-pin/1`: `repo` (owner/name), `trunk`, `mode` (`whole` or
  `per-layer`), and `layers`, bottom to top, each `pr`, `title`, `url`, `branch`, `base`, `head`,
  and `body` (the PR description when the brief was pinned; empty with `--layers`).
- **index.json**, `heph-review-brief-index/1`: `pin`; `layers`, each `pr`, `title`, `base`,
  `head`, `fingerprint`, `total`, `live`, `budget` (`lines`, `cards`), `live_ranges` (a RangeRef
  plus `lines`, `flags`, `symbol`, `floor`), `counted` (class to line count), `weakenings`,
  `description` (the pinned PR description) and `commits` (the range's commits, oldest first,
  each `sha`, `subject`, `body`); `index.md` quotes both as data;
  `duplicates` (pairs of LayerRef `a`, `b`); `files_touched_by_several_layers` (`path`,
  `layers`). `index.md` is the same content as text, one section per layer.

**Weakening**: `id`, `kind` (`deleted-test`, `new-ignore`, `lost-assertions`,
`missing-spec-test`), `test`, `range` (optional RangeRef), `detail`. Ids are
`<kind>:<path>:<test>`, or `missing-spec-test:<test>`. A missing spec test belongs to the top
layer.

**Fingerprint**: a hash of the layer's files and its classified changed lines, without SHAs. A
rebase keeps it; a changed diff or a changed set of live lines moves it.

## The page data

`brief render` writes these into its output directory, through a temp directory swapped in
whole, so a refused render changes nothing. Every file has `format: "heph-review-brief/1"`.

### manifest.json

| Field | Type | Meaning |
| --- | --- | --- |
| `files` | map | File name to sha256 (hex) of its bytes, for every file below except `manifest.json` and `index.html` |
| `sums` | map | Layer PR number (as a string) to its Sums |

The page refuses to draw the ledger when a file's hash or a layer's sums do not match.

**Sums**: `total` (the layer's changed lines, equal to `git diff --numstat`), `scrutinize`,
`glance`, `ai_cleared`, `ai_cleared_read_by_decider_or_stack_review`,
`ai_cleared_read_by_scout_only`, `counted`, `untriaged`. The tiers, `counted` and `untriaged`
sum to `total`; the two `ai_cleared_read_by_*` split `ai_cleared`.

### stack.json

| Field | Type | Meaning |
| --- | --- | --- |
| `repo`, `trunk`, `mode` | string | From the pin |
| `stack_key` | string | `<repo>#<bottom PR>`: what the page keys its browser state by |
| `freshness_api` | string | Where the page asks for each PR's current head |
| `layers` | LayerHead[] | Bottom to top |
| `assessment` | object, optional | The stack reviewer's `text` and `refs` |
| `decisions` | object[] | "Decisions waiting for you": `layer`, `card` (an id), `text` |
| `not_in_code` | NotInCode[] | From the triage |
| `open_cards` | int | Scrutinize cards in the stack |

**LayerHead**: the pin's layer fields (`pr`, `title`, `url`, `branch`, `base`, `head`, `body`), plus `file`
(its `layer-N.json`), `diffs_file` (its `layer-N-diffs.json`), `sums`, and `scrutinize_cards`.

### layer-N.json

One per layer, N its PR number.

| Field | Type | Meaning |
| --- | --- | --- |
| `pr` | int | |
| `sums` | Sums | |
| `live` | int | Live lines |
| `budget`, `used` | `{lines, cards}` | The Scrutinize budget, and what the cards use |
| `overflow` | bool | `used` is over `budget`. Every card is still in `cards` (I7) |
| `confirmation` | string, optional | The stack reviewer's AI-approved line |
| `cards` | Card[] | Every item of the layer, highest tier first |
| `untriaged` | PageRange[] | Live lines no item covers. Empty on a valid brief |
| `counted` | Counted[] | One row per file and class |
| `weakenings` | Weakening[] | What the program found |
| `fingerprint` | string | See above |

**Card**: `id`, `tier`, `reason`, `reason_title`, `title`, `context`, `claim`, `question`,
`options`, `diagram`, `examples`, `display`, `changed`
(changed lines under its ranges), `ranges` (PageRange[]), `used_by` (PageUse[]), `start_from`,
`tests` (each `name` and
`link`), `board` (BoardNote[]), `weakenings`, `provenance` (`proposed_by`, `tiered_by`,
`raised_by`), `other_layers` (each `pr`, `symbol`, `link`: other layers that change the same
function), `floors` (the flags that held a line at this tier, such as `protected`).

`display` is `embed` (up to 80 changed lines: the diff is in the diffs file), `links` (larger:
read the ranges in order), or `file` (a whole new or rewritten file: start from `start_from`).

**PageRange**: a RangeRef's fields, plus:

| Field | Type | Meaning |
| --- | --- | --- |
| `hash` | string | Content hash of the added and removed lines (I8). Review state follows it. A repeat of the same block gets a `-1`, `-2` suffix |
| `context` | string | Hash of the three lines on each side. A changed value with the same `hash` means "surroundings changed" |
| `anchor` | string | `path#symbol`: the range's identity across rounds |
| `link` | string | The lines in the PR's Files view, at the pinned range |
| `changed` | int | Changed lines in the range |
| `commits` | object[] | Each `sha`, `short`, `subject`, `link`: the commits in the pinned range that last touched these lines |
| `diff` | bool | Its lines are in the diffs file under `hash` |

**PageUse**: `path`, `start`, `end`, `note`, `symbol` (the function or type the site is in),
`link` (the lines on GitHub at the layer's pinned head) and `key` (its snippet in the diffs
file).

**Counted**: `path`, `class` (`test`, `moved`, `docs`, `lockfile`, `generated`, `superseded`,
`binary`, `symlink`, `submodule`, `mode`), `added`, `removed`, `link`, `moved` (the other end of
moved lines, as `path:line`), `note` (a rename, a mode change, or how many existing lockfile pins
moved).

### layer-N-diffs.json

| Field | Type | Meaning |
| --- | --- | --- |
| `pr` | int | |
| `ranges` | map | PageRange `hash` to its diff lines |
| `snippets` | map | PageUse `key` to its lines |

A diff line is a four-element array `[kind, old, new, text]`. `kind` is `" "` (context), `"+"`,
`"-"`, or `"@"` for a gap between non-adjacent lines (then `old` and `new` are 0 and `text` is
empty). `old` and `new` are the line numbers on each side, 0 where the line has none. The text is
cut from git at the pinned SHAs (I2).

A snippet line has the same shape: `kind` is `">"` for a line of the cited range and `" "` for
the three lines around it, `old` is 0 and `new` is the line number at the layer's head. The key is
a hash of the path and the lines, so a site two cards cite is stored once. The snippets are in the
diffs file, so the manifest's checksum of that file covers them.

### index.html

The page itself, `page.html` copied unchanged. It loads `manifest.json` first, then
`stack.json`, then each layer's two files on demand.

**Diagrams are the one place the page inserts generated markup.** Everything else is inserted as
text. A card's `diagram` is handed to mermaid 11.12.0, loaded from cdnjs with an SRI `integrity`
hash, initialized with `securityLevel: "strict"` and `startOnLoad: false`, and run only when the
card is drawn. Its SVG, which strict mode sanitizes, is parsed with `DOMParser`, stripped again of
scripts, `on*` attributes and `javascript:` values, and adopted as a node. This is accepted
because a picture of a flow is worth it, the source is already held to a subset with no markup,
links or directives (render refuses the rest), and the script is pinned by hash. The brief never
depends on the CDN: until mermaid draws, and whenever it fails to load or to parse, the source is
shown as preformatted text.
