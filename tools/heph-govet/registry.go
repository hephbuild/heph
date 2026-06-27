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
func registry() map[string][]*analysis.Analyzer {
	r := map[string][]*analysis.Analyzer{
		// govet — the standard `go vet` analyzer set.
		"govet": {
			appends.Analyzer, asmdecl.Analyzer, assign.Analyzer, atomic.Analyzer,
			bools.Analyzer, buildtag.Analyzer, cgocall.Analyzer, composite.Analyzer,
			copylock.Analyzer, defers.Analyzer, directive.Analyzer, errorsas.Analyzer,
			httpresponse.Analyzer, ifaceassert.Analyzer, loopclosure.Analyzer,
			lostcancel.Analyzer, nilfunc.Analyzer, printf.Analyzer, shift.Analyzer,
			sigchanyzer.Analyzer, slog.Analyzer, stdmethods.Analyzer,
			stringintconv.Analyzer, structtag.Analyzer, testinggoroutine.Analyzer,
			tests.Analyzer, timeformat.Analyzer, unmarshal.Analyzer,
			unreachable.Analyzer, unsafeptr.Analyzer, unusedresult.Analyzer,
		},
		"staticcheck": honnefAnalyzers(staticcheck.Analyzers),
		"gosimple":    honnefAnalyzers(simple.Analyzers),
		"stylecheck":  honnefAnalyzers(stylecheck.Analyzers),
	}
	return r
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
