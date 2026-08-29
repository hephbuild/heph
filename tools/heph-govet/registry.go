package main

import (
	"golang.org/x/tools/go/analysis"
	"golang.org/x/tools/go/analysis/suite/vet"

	// Off-by-default vet passes (as in `go vet` / golangci): opt-in via
	// `linters.settings.govet.enable` / `enable-all`. These are the only passes
	// named here — the default-on set comes from `vet.Suite`, see govetAll.
	"golang.org/x/tools/go/analysis/passes/fieldalignment"
	"golang.org/x/tools/go/analysis/passes/nilness"
	"golang.org/x/tools/go/analysis/passes/shadow"
	"golang.org/x/tools/go/analysis/passes/sortslice"
	"golang.org/x/tools/go/analysis/passes/unusedwrite"

	"honnef.co/go/tools/analysis/lint"
	"honnef.co/go/tools/simple"
	"honnef.co/go/tools/staticcheck"
	"honnef.co/go/tools/stylecheck"
)

// registry maps a golangci-lint linter NAME to the set of upstream
// *analysis.Analyzer it corresponds to. golangci-lint is a curator over these
// same upstream analyzers; we replicate the mapping for the supported subset so
// a `.golangci.yml` that enables/disables linters by name selects the right
// analyzers here.
//
// To add a linter: import the upstream package that EXPORTS its analyzers and
// add an entry. Linters whose checks are not go/analysis analyzers (formatters
// like gofmt/gofumpt/goimports) or that need whole-program analysis (`unused`)
// cannot run in the per-package unitchecker model and are intentionally absent —
// see the package doc in main.go.
func registry(cfg golangciConfig) map[string][]*analysis.Analyzer {
	s := cfg.Linters.Settings
	return map[string][]*analysis.Analyzer{
		"govet":       resolveGovet(s.Govet),
		"staticcheck": filterChecks(honnefAnalyzers(staticcheck.Analyzers), s.Staticcheck.Checks),
		"gosimple":    filterChecks(honnefAnalyzers(simple.Analyzers), s.Gosimple.Checks),
		"stylecheck":  filterChecks(honnefAnalyzers(stylecheck.Analyzers), s.Stylecheck.Checks),
	}
}

// govetEntry is a vet analyzer plus whether it belongs to the default `govet`
// group. Off-by-default entries are opt-in via `settings.govet`.
type govetEntry struct {
	a         *analysis.Analyzer
	defaultOn bool
}

// govetAll is every vet analyzer this binary can run, in deterministic order:
// `go vet`'s own suite first (default on), then the extras it deliberately
// leaves out (default off, opt-in via settings).
//
// The default-on set *is* x/tools' `suite/vet.Suite` — the same slice `cmd/vet`
// runs (`unitchecker.Main(vet.Suite...)`), not a transcription of it. It used to
// be spelled out here, and by the time Go 1.27 landed it had drifted six
// analyzers behind what `go vet` actually runs: framepointer, hostport,
// scannererr, sqlrowserr, stdversion and waitgroup. Deriving it makes "matches
// `go vet`" true by construction, so the next Go release's new checks arrive
// wired rather than waiting for someone to notice the gap.
//
// Selection and settings are keyed by analyzer *name* (see resolveGovet), so a
// suite that grows upstream needs no change here — a `.golangci.yml` naming a
// new analyzer just starts working.
func govetAll() []govetEntry {
	// Off by default, as in `go vet` and golangci: noisy or expensive enough
	// that an always-on set is the wrong default. The upstream suite omits
	// fieldalignment by name for exactly that reason.
	off := []*analysis.Analyzer{
		shadow.Analyzer, fieldalignment.Analyzer, nilness.Analyzer,
		sortslice.Analyzer, unusedwrite.Analyzer,
	}
	out := make([]govetEntry, 0, len(vet.Suite)+len(off))
	inSuite := make(map[string]bool, len(vet.Suite))
	for _, a := range vet.Suite {
		out = append(out, govetEntry{a, true})
		inSuite[a.Name] = true
	}
	for _, a := range off {
		// Upstream can promote one of these into the suite — hostport and
		// waitgroup arrived in `go vet` exactly that way. Appending it anyway
		// would shadow the default-on entry, because resolveGovet keys by name
		// and takes the last one it sees: a `go vet` default would silently
		// switch itself off on an x/tools bump. The suite wins.
		if inSuite[a.Name] {
			continue
		}
		out = append(out, govetEntry{a, false})
	}
	return out
}

// honnefAnalyzers unwraps honnef.co/go/tools' lint.Analyzer wrappers into bare
// *analysis.Analyzer.
func honnefAnalyzers(in []*lint.Analyzer) []*analysis.Analyzer {
	out := make([]*analysis.Analyzer, 0, len(in))
	for _, a := range in {
		out = append(out, a.Analyzer)
	}
	return out
}

// defaultStandard is golangci-lint's `default: standard` linter group.
var defaultStandard = []string{"govet"}

// defaultAll is every linter known to this binary.
func defaultAll(r map[string][]*analysis.Analyzer) []string {
	names := make([]string, 0, len(r))
	for name := range r {
		names = append(names, name)
	}
	return names
}
