package main

import (
	"fmt"
	"os"
	"sort"

	"golang.org/x/tools/go/analysis"
	"gopkg.in/yaml.v3"
)

// golangciConfig is the minimal subset of `.golangci.yml` (schema version 2)
// this driver understands: linter selection. Settings, exclusions, formatters
// and `//nolint` handling are not parsed here yet — see the package doc.
type golangciConfig struct {
	Version string `yaml:"version"`
	Linters struct {
		// "standard" (default), "all", "none", or "fast". Only standard/all/none
		// are meaningful here; "fast" is treated as "standard".
		Default    string   `yaml:"default"`
		Enable     []string `yaml:"enable"`
		Disable    []string `yaml:"disable"`
		Exclusions struct {
			// Subset of golangci-lint exclusions: per-rule linter scoping plus
			// path/text regexes. Presets and `source` matching are not parsed.
			Rules []struct {
				Linters []string `yaml:"linters"`
				Path    string   `yaml:"path"`
				Text    string   `yaml:"text"`
			} `yaml:"rules"`
		} `yaml:"exclusions"`
	} `yaml:"linters"`
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
				linterOf[a] = n
				out = append(out, a)
			}
		}
	}
	sort.Strings(unknown)
	return out, linterOf, unknown, nil
}
