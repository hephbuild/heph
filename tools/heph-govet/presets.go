package main

// This file encodes golangci-lint's built-in exclusion presets — the `EXCxxxx`
// default-exclude patterns a user opts into via `linters.exclusions.presets`.
//
// Each pattern is a (linters, text-regex) exclude rule tagged with the preset
// group it belongs to. golangci-lint v2 folded gosimple/stylecheck into
// `staticcheck`, so honnef patterns are scoped to `staticcheck` here (matching
// [`nolintLinterName`]). Patterns scoped to linters heph-govet does not run
// (errcheck, gosec, revive, golint, gocritic) are kept for fidelity but are
// inert — those findings never appear, so the rule simply never matches.
//
// Preset names mirror golangci-lint: "comments", "common-false-positives",
// "legacy", "std-error-handling".

// presetPattern is one built-in default-exclude entry.
type presetPattern struct {
	id      string   // golangci EXC id, for reference
	preset  string   // the preset group this belongs to
	linters []string // golangci linter names it is scoped to (v2 names)
	text    string   // message regex it matches
}

// presetPatterns is golangci-lint's default exclude list. The `text` regexes and
// linter scoping are taken from golangci-lint's documented defaults; the honnef
// ones are re-scoped to the v2 `staticcheck` name.
var presetPatterns = []presetPattern{
	// --- std-error-handling: unchecked errors on common stdlib calls ---
	{
		id:      "EXC0001",
		preset:  "std-error-handling",
		linters: []string{"errcheck"},
		text:    `Error return value of .((os\.)?std(out|err)\..*|.*Close|.*Flush|os\.Remove(All)?|.*print(f|ln)?|os\.(Un)?Setenv). is not checked`,
	},

	// --- legacy: the original golangci default excludes ---
	{
		id:      "EXC0002",
		preset:  "legacy",
		linters: []string{"golint"},
		text:    `(comment on exported (method|function|type|const)|should have( a package)? comment|comment should be of the form)`,
	},
	{
		id:      "EXC0003",
		preset:  "legacy",
		linters: []string{"golint"},
		text:    `func name will be used as test\.Test.* by other packages, and that stutters; consider calling this`,
	},
	{
		id:      "EXC0004",
		preset:  "legacy",
		linters: []string{"govet"},
		text:    `(possible misuse of unsafe\.Pointer|should have signature)`,
	},
	{
		id:      "EXC0005",
		preset:  "legacy",
		linters: []string{"staticcheck"},
		text:    `ineffective break statement\. Did you mean to break out of the outer loop`,
	},
	{
		id:      "EXC0006",
		preset:  "legacy",
		linters: []string{"gosec"},
		text:    `Use of unsafe calls should be audited`,
	},
	{
		id:      "EXC0007",
		preset:  "legacy",
		linters: []string{"gosec"},
		text:    `Subprocess launch(ed with variable|ing should be audited)`,
	},
	{
		id:      "EXC0008",
		preset:  "legacy",
		linters: []string{"gosec"},
		text:    `(G104|G307)`,
	},
	{
		id:      "EXC0009",
		preset:  "legacy",
		linters: []string{"gosec"},
		text:    `(Expect directory permissions to be 0750 or less|Expect file permissions to be 0600 or less)`,
	},
	{
		id:      "EXC0010",
		preset:  "legacy",
		linters: []string{"gosec"},
		text:    `Potential file inclusion via variable`,
	},

	// --- comments: exported/package comment style rules ---
	{
		id:      "EXC0011",
		preset:  "comments",
		linters: []string{"staticcheck"},
		text:    `(comment on exported (method|function|type|const)|should have( a package)? comment|comment should be of the form)`,
	},
	{
		id:      "EXC0012",
		preset:  "comments",
		linters: []string{"revive"},
		text:    `exported (.+) should have comment( \(or a comment on this block\))? or be unexported`,
	},
	{
		id:      "EXC0013",
		preset:  "comments",
		linters: []string{"revive"},
		text:    `package comment should be of the form "(.+)..."`,
	},
	{
		id:      "EXC0014",
		preset:  "comments",
		linters: []string{"revive"},
		text:    `comment on exported (.+) should be of the form "(.+)..."`,
	},
	{
		id:      "EXC0015",
		preset:  "comments",
		linters: []string{"revive"},
		text:    `should have a package comment`,
	},

	// --- common-false-positives ---
	{
		id:      "EXC0016",
		preset:  "common-false-positives",
		linters: []string{"gocritic"},
		text:    `(commentFormatting|exitAfterDefer|ifElseChain)`,
	},
}

// knownPresets is the set of preset names golangci-lint defines. A preset not in
// this set is a config error (fail loudly rather than silently do nothing).
var knownPresets = map[string]bool{
	"comments":               true,
	"common-false-positives": true,
	"legacy":                 true,
	"std-error-handling":     true,
}
