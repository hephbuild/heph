// Command heph-govet is a per-package go/analysis driver, built on
// golang.org/x/tools/go/analysis/unitchecker, with golangci-lint-style linter
// selection from a `.golangci.yml`.
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
// That is the nogo / `go vet` model. It gives heph tight per-package caching and
// full interprocedural fidelity (e.g. printf wrapper detection across package
// boundaries), which export-data-only linting loses.
//
// # Linters
//
// golangci-lint does not own its analyzers — it curates upstream go/analysis
// packages. This binary imports those same upstream analyzers directly (see
// registry.go) and selects among them from a `.golangci.yml` (linters
// enable/disable/default). The config path is passed via the
// HEPH_GOVET_GOLANGCI_CONFIG environment variable by the heph go_lint driver.
//
// `//nolint` directives and `linters.exclusions.rules` (path/text/linter
// scoping) ARE honored: each analyzer's emission is wrapped with a suppressor
// (see filter.go), so the report this binary writes is already filtered.
//
// Not supported in this per-package model (by design):
//   - formatters (gofmt, gofumpt, goimports) — not go/analysis analyzers;
//   - `unused` — needs whole-program analysis, incompatible with per-unit facts;
//   - exclusion presets and `exclusions.rules[].source` matching (only
//     path/text/linter scoping is parsed).
//
// # Invocation (by the heph go_lint driver)
//
//	HEPH_GOVET_GOLANGCI_CONFIG=/path/.golangci.yml heph-govet -json <cfg.cfg>
//
// unitchecker requires the cfg argument to end in ".cfg". With -json it writes
// facts to cfg.VetxOutput and prints diagnostics as JSON to stdout, exiting 0
// even when findings exist; the heph gate target decides pass/fail.
package main

import (
	"fmt"
	"os"

	"golang.org/x/tools/go/analysis/unitchecker"
)

// configEnvVar names the environment variable carrying the `.golangci.yml` path.
const configEnvVar = "HEPH_GOVET_GOLANGCI_CONFIG"

func main() {
	r := registry()
	cfg, err := loadConfig(os.Getenv(configEnvVar))
	if err != nil {
		fatal(err.Error())
	}
	analyzers, linterOf, unknown, err := selectAnalyzers(cfg, r)
	if err != nil {
		fatal(err.Error())
	}
	// Fail loudly on linters we can't run rather than silently dropping them.
	if len(unknown) > 0 {
		fatal(fmt.Sprintf(
			"unsupported linters in %s: %v\n(formatters and whole-program linters "+
				"like `unused` are not available in the per-package model; see heph-govet docs)",
			configEnvVar, unknown,
		))
	}
	if len(analyzers) == 0 {
		fatal("no analyzers enabled (check linters.default / linters.enable)")
	}
	if os.Getenv("HEPH_GOVET_DEBUG") != "" {
		for _, a := range analyzers {
			fmt.Fprintf(os.Stderr, "heph-govet: enabled analyzer %s\n", a.Name)
		}
	}

	// Apply `//nolint` + exclusion filtering by wrapping each analyzer's emission.
	sup, err := newSuppressor(cfg)
	if err != nil {
		fatal(err.Error())
	}
	analyzers = wrapAnalyzers(analyzers, linterOf, sup)

	unitchecker.Main(analyzers...)
}

func fatal(msg string) {
	fmt.Fprintf(os.Stderr, "heph-govet: %s\n", msg)
	os.Exit(2)
}
