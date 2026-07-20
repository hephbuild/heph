package main

import (
	"golang.org/x/tools/go/analysis"

	"golang.org/x/tools/go/analysis/passes/appends"
	"golang.org/x/tools/go/analysis/passes/asmdecl"
	"golang.org/x/tools/go/analysis/passes/assign"
	"golang.org/x/tools/go/analysis/passes/atomic"
	"golang.org/x/tools/go/analysis/passes/bools"
	"golang.org/x/tools/go/analysis/passes/buildtag"
	"golang.org/x/tools/go/analysis/passes/cgocall"
	"golang.org/x/tools/go/analysis/passes/composite"
	"golang.org/x/tools/go/analysis/passes/copylock"
	"golang.org/x/tools/go/analysis/passes/defers"
	"golang.org/x/tools/go/analysis/passes/directive"
	"golang.org/x/tools/go/analysis/passes/errorsas"
	"golang.org/x/tools/go/analysis/passes/httpresponse"
	"golang.org/x/tools/go/analysis/passes/ifaceassert"
	"golang.org/x/tools/go/analysis/passes/loopclosure"
	"golang.org/x/tools/go/analysis/passes/lostcancel"
	"golang.org/x/tools/go/analysis/passes/nilfunc"
	"golang.org/x/tools/go/analysis/passes/printf"
	"golang.org/x/tools/go/analysis/passes/shift"
	"golang.org/x/tools/go/analysis/passes/sigchanyzer"
	"golang.org/x/tools/go/analysis/passes/slog"
	"golang.org/x/tools/go/analysis/passes/stdmethods"
	"golang.org/x/tools/go/analysis/passes/stringintconv"
	"golang.org/x/tools/go/analysis/passes/structtag"
	"golang.org/x/tools/go/analysis/passes/testinggoroutine"
	"golang.org/x/tools/go/analysis/passes/tests"
	"golang.org/x/tools/go/analysis/passes/timeformat"
	"golang.org/x/tools/go/analysis/passes/unmarshal"
	"golang.org/x/tools/go/analysis/passes/unreachable"
	"golang.org/x/tools/go/analysis/passes/unsafeptr"
	"golang.org/x/tools/go/analysis/passes/unusedresult"

	// Off-by-default vet passes (as in `go vet` / golangci): opt-in via
	// `linters.settings.govet.enable` / `enable-all`.
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

// govetAll is every vet analyzer this binary can run, in deterministic order.
// The default-on set matches `go vet`'s standard analyzers; the trailing few are
// off by default (as in `go vet` / golangci) and enabled via settings.
func govetAll() []govetEntry {
	on := func(a *analysis.Analyzer) govetEntry { return govetEntry{a, true} }
	off := func(a *analysis.Analyzer) govetEntry { return govetEntry{a, false} }
	return []govetEntry{
		on(appends.Analyzer), on(asmdecl.Analyzer), on(assign.Analyzer), on(atomic.Analyzer),
		on(bools.Analyzer), on(buildtag.Analyzer), on(cgocall.Analyzer), on(composite.Analyzer),
		on(copylock.Analyzer), on(defers.Analyzer), on(directive.Analyzer), on(errorsas.Analyzer),
		on(httpresponse.Analyzer), on(ifaceassert.Analyzer), on(loopclosure.Analyzer),
		on(lostcancel.Analyzer), on(nilfunc.Analyzer), on(printf.Analyzer), on(shift.Analyzer),
		on(sigchanyzer.Analyzer), on(slog.Analyzer), on(stdmethods.Analyzer),
		on(stringintconv.Analyzer), on(structtag.Analyzer), on(testinggoroutine.Analyzer),
		on(tests.Analyzer), on(timeformat.Analyzer), on(unmarshal.Analyzer),
		on(unreachable.Analyzer), on(unsafeptr.Analyzer), on(unusedresult.Analyzer),
		off(shadow.Analyzer), off(fieldalignment.Analyzer), off(nilness.Analyzer),
		off(sortslice.Analyzer), off(unusedwrite.Analyzer),
	}
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
