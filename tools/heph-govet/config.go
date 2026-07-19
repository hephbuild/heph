package main

import (
	"fmt"
	"os"
	"sort"

	"golang.org/x/tools/go/analysis"
	"gopkg.in/yaml.v3"
)

// golangciConfig is the subset of `.golangci.yml` (schema version 2) this driver
// understands: linter selection, per-linter `settings`, exclusions, and the
// `formatters` block. `run:`/`output:`/`severity:`/`issues:` and
// `exclusions.rules[].source` are not parsed — see the package doc.
type golangciConfig struct {
	Version string `yaml:"version"`
	Linters struct {
		// "standard" (default), "all", "none", or "fast". Only standard/all/none
		// are meaningful here; "fast" is treated as "standard".
		Default    string           `yaml:"default"`
		Enable     []string         `yaml:"enable"`
		Disable    []string         `yaml:"disable"`
		Settings   linterSettings   `yaml:"settings"`
		Exclusions exclusionsConfig `yaml:"exclusions"`
	} `yaml:"linters"`
	Formatters formattersConfig `yaml:"formatters"`
}

// exclusionsConfig mirrors golangci-lint v2's `linters.exclusions`: generated-
// file handling, built-in exclusion presets, per-rule scoping, and whole-file
// path filters. `rules[].source` matching is the only sub-feature not parsed.
type exclusionsConfig struct {
	// "lax" (default), "strict", or "disable" — how generated files are
	// detected and excluded. Empty is treated as "lax", matching golangci.
	Generated string `yaml:"generated"`
	// Built-in exclusion presets: "comments", "common-false-positives",
	// "legacy", "std-error-handling". Each expands to a set of exclude rules.
	Presets []string          `yaml:"presets"`
	Rules   []excludeRuleYAML `yaml:"rules"`
	// Whole-file path regexes: `paths` excludes issues in matching files;
	// `paths-except` excludes issues in files that match NONE of them.
	Paths       []string `yaml:"paths"`
	PathsExcept []string `yaml:"paths-except"`
}

// excludeRuleYAML is one `exclusions.rules` entry. All present conditions are
// AND-ed; `path-except` negates (the rule does not apply to matching paths).
type excludeRuleYAML struct {
	Linters    []string `yaml:"linters"`
	Path       string   `yaml:"path"`
	PathExcept string   `yaml:"path-except"`
	Text       string   `yaml:"text"`
}

// linterSettings is `linters.settings` — per-linter configuration for the
// families this driver ships.
type linterSettings struct {
	Govet       govetSettings  `yaml:"govet"`
	Staticcheck honnefSettings `yaml:"staticcheck"`
	Gosimple    honnefSettings `yaml:"gosimple"`
	Stylecheck  honnefSettings `yaml:"stylecheck"`
}

// govetSettings mirrors golangci-lint's `linters.settings.govet`: which vet
// analyzers run, and per-analyzer flag values (e.g. `printf.funcs`).
type govetSettings struct {
	Enable     []string `yaml:"enable"`
	Disable    []string `yaml:"disable"`
	EnableAll  bool     `yaml:"enable-all"`
	DisableAll bool     `yaml:"disable-all"`
	// analyzer name → (flag name → value). Values are coerced to the flag's
	// string form (lists join on comma, bools to "true"/"false").
	Settings map[string]map[string]any `yaml:"settings"`
}

// honnefSettings mirrors golangci-lint's `linters.settings.{staticcheck,
// gosimple,stylecheck}`. `checks` selects which checks in the family run
// (patterns: "all", "SA1000", "-ST1003", globs like "SA1*"); the remaining
// fields feed honnef's config (consumed by stylecheck's ST1001/ST1003/ST1013).
type honnefSettings struct {
	Checks                  []string `yaml:"checks"`
	Initialisms             []string `yaml:"initialisms"`
	DotImportWhitelist      []string `yaml:"dot-import-whitelist"`
	HTTPStatusCodeWhitelist []string `yaml:"http-status-code-whitelist"`
}

// formattersConfig is the `formatters:` block: which formatters run and their
// settings. Mirrors golangci-lint v2 for gofmt/gofumpt/goimports.
type formattersConfig struct {
	Enable   []string `yaml:"enable"`
	Settings struct {
		Gofmt struct {
			Simplify     bool `yaml:"simplify"`
			RewriteRules []struct {
				Pattern     string `yaml:"pattern"`
				Replacement string `yaml:"replacement"`
			} `yaml:"rewrite-rules"`
		} `yaml:"gofmt"`
		Gofumpt struct {
			ModulePath string `yaml:"module-path"`
			ExtraRules bool   `yaml:"extra-rules"`
		} `yaml:"gofumpt"`
		Goimports struct {
			LocalPrefixes []string `yaml:"local-prefixes"`
		} `yaml:"goimports"`
	} `yaml:"settings"`
}

// loadConfig reads and parses a `.golangci.yml` at path. A missing path yields
// the zero config (→ the standard set), so an unconfigured repo still lints.
func loadConfig(path string) (golangciConfig, error) {
	var cfg golangciConfig
	if path == "" {
		return cfg, nil
	}
	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return cfg, nil
		}
		return cfg, fmt.Errorf("read %s: %w", path, err)
	}
	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse %s: %w", path, err)
	}
	return cfg, nil
}

// selectAnalyzers resolves the enabled linter set from cfg against the registry,
// then flattens to the concrete analyzers to run. It also returns the
// analyzer→golangci-linter-name map (for `//nolint:<linter>` matching). Unknown
// linter names (e.g. a formatter or a linter not yet mapped here) are reported
// so they fail loudly rather than being silently ignored.
func selectAnalyzers(cfg golangciConfig, r map[string][]*analysis.Analyzer) ([]*analysis.Analyzer, map[*analysis.Analyzer]string, []string, error) {
	enabled := map[string]bool{}
	switch cfg.Linters.Default {
	case "", "standard", "fast":
		for _, n := range defaultStandard {
			enabled[n] = true
		}
	case "all":
		for _, n := range defaultAll(r) {
			enabled[n] = true
		}
	case "none":
		// start empty
	default:
		return nil, nil, nil, fmt.Errorf("unknown linters.default %q", cfg.Linters.Default)
	}

	var unknown []string
	for _, n := range cfg.Linters.Enable {
		if _, ok := r[n]; !ok {
			unknown = append(unknown, n)
			continue
		}
		enabled[n] = true
	}
	for _, n := range cfg.Linters.Disable {
		delete(enabled, n)
	}

	// Deterministic order: by linter name, analyzers in registry order.
	names := make([]string, 0, len(enabled))
	for n := range enabled {
		names = append(names, n)
	}
	sort.Strings(names)

	seen := map[*analysis.Analyzer]bool{}
	linterOf := map[*analysis.Analyzer]string{}
	var out []*analysis.Analyzer
	for _, n := range names {
		for _, a := range r[n] {
			if !seen[a] {
				seen[a] = true
				linterOf[a] = nolintLinterName(n)
				out = append(out, a)
			}
		}
	}
	sort.Strings(unknown)
	return out, linterOf, unknown, nil
}

// nolintLinterName maps a registry linter name to the golangci-lint v2 linter
// name used for `//nolint` and `exclusions.rules` matching. golangci v2 removed
// the separate `gosimple` and `stylecheck` linters and folded their checks into
// `staticcheck`, so every honnef check (SA/S/ST) is suppressed with
// `//nolint:staticcheck` — the name heph-govet reports for these findings too.
func nolintLinterName(registryName string) string {
	switch registryName {
	case "staticcheck", "gosimple", "stylecheck":
		return "staticcheck"
	default:
		return registryName
	}
}
