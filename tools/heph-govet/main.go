// Command heph-govet is a per-package go/analysis driver, built on
// golang.org/x/tools/go/analysis/unitchecker.
//
// It is the per-package "unit" checker heph runs for the `lint` target. Unlike
// golangci-lint (which loads the whole package graph in one process and keeps
// analysis facts only in memory), unitchecker analyzes exactly ONE package per
// invocation:
//
//   - dependency type info comes from compiled export data (.a archives), wired
//     in via the cfg's PackageFile map — never by re-loading dep sources;
//   - dependency analysis FACTS come from each dep's serialized facts file, wired
//     in via the cfg's PackageVetx map;
//   - this package's own facts are written to cfg.VetxOutput, to be consumed by
//     dependents.
//
// That is the nogo / `go vet` model. It gives heph tight per-package caching: a
// package re-lints only when its own sources, a dep's export data, or a dep's
// exported facts change — and full interprocedural fidelity (e.g. printf wrapper
// detection across package boundaries), which export-data-only linting loses.
//
// Invocation (by the heph go_lint driver):
//
//	heph-govet -json <cfg.json>
//
// With -json, unitchecker writes facts to cfg.VetxOutput and prints the
// diagnostics as JSON to stdout, exiting 0 even when findings exist. The heph
// driver captures that JSON as the lint report; a separate gate target fails the
// build when the report is non-empty (so facts are always produced and cached,
// even for a package that currently has findings).
package main

import (
	"golang.org/x/tools/go/analysis/unitchecker"

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
)

func main() {
	// The `go vet` default analyzer set. The fact-producing/consuming passes
	// (printf, lostcancel, nilfunc, unusedresult, stringintconv, …) are the ones
	// that need the PackageVetx wiring to reason across package boundaries.
	//
	// To add custom or third-party analyzers (anything exposing an
	// *analysis.Analyzer — including most golangci-lint native linters and tools
	// like nilaway), append them here and rebuild the binary. No other change is
	// needed: facts flow automatically for any Analyzer that uses them.
	unitchecker.Main(
		appends.Analyzer,
		asmdecl.Analyzer,
		assign.Analyzer,
		atomic.Analyzer,
		bools.Analyzer,
		buildtag.Analyzer,
		cgocall.Analyzer,
		composite.Analyzer,
		copylock.Analyzer,
		defers.Analyzer,
		directive.Analyzer,
		errorsas.Analyzer,
		httpresponse.Analyzer,
		ifaceassert.Analyzer,
		loopclosure.Analyzer,
		lostcancel.Analyzer,
		nilfunc.Analyzer,
		printf.Analyzer,
		shift.Analyzer,
		sigchanyzer.Analyzer,
		slog.Analyzer,
		stdmethods.Analyzer,
		stringintconv.Analyzer,
		structtag.Analyzer,
		testinggoroutine.Analyzer,
		tests.Analyzer,
		timeformat.Analyzer,
		unmarshal.Analyzer,
		unreachable.Analyzer,
		unsafeptr.Analyzer,
		unusedresult.Analyzer,
	)
}
