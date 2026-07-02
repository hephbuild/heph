package main

import (
	"fmt"
	"path"
	"sort"
	"strconv"
	"strings"

	"golang.org/x/tools/go/analysis"
	"honnef.co/go/tools/config"
)

// resolveGovet builds the `govet` family analyzer set from settings: start from
// the default-on vet analyzers, then apply disable-all/enable-all and the
// per-name enable/disable lists (enable wins over disable, mirroring golangci).
func resolveGovet(s govetSettings) []*analysis.Analyzer {
	all := govetAll()
	enabled := make(map[string]bool, len(all))
	for _, e := range all {
		switch {
		case s.DisableAll:
			enabled[e.a.Name] = false
		case s.EnableAll:
			enabled[e.a.Name] = true
		default:
			enabled[e.a.Name] = e.defaultOn
		}
	}
	for _, n := range s.Disable {
		enabled[n] = false
	}
	for _, n := range s.Enable {
		enabled[n] = true
	}

	out := make([]*analysis.Analyzer, 0, len(all))
	for _, e := range all {
		if enabled[e.a.Name] {
			out = append(out, e.a)
		}
	}
	return out
}

// filterChecks narrows a honnef family's analyzers by golangci `checks`
// patterns. Empty → every check runs (honnef's "all" default). Otherwise the
// patterns are applied in order over the family's check names (the analyzer
// Name, e.g. "SA1000"): "all"/"*" enable everything, "none" clear, a bare
// pattern enables matches, a "-"-prefixed pattern disables them. Patterns may be
// globs (path.Match semantics, e.g. "SA1*", "ST10??").
func filterChecks(in []*analysis.Analyzer, checks []string) []*analysis.Analyzer {
	if len(checks) == 0 {
		return in
	}
	enabled := make(map[string]bool, len(in))
	apply := func(pat string, on bool) {
		switch pat {
		case "all", "*":
			for _, a := range in {
				enabled[a.Name] = on
			}
			return
		case "none":
			for _, a := range in {
				enabled[a.Name] = false
			}
			return
		}
		for _, a := range in {
			if ok, _ := path.Match(pat, a.Name); ok {
				enabled[a.Name] = on
			}
		}
	}
	for _, c := range checks {
		if strings.HasPrefix(c, "-") {
			apply(strings.TrimPrefix(c, "-"), false)
		} else {
			apply(c, true)
		}
	}
	out := make([]*analysis.Analyzer, 0, len(in))
	for _, a := range in {
		if enabled[a.Name] {
			out = append(out, a)
		}
	}
	return out
}

// applyGovetFlags sets per-analyzer flag values from `settings.govet.settings`
// on the selected analyzers. Because heph-govet's argv carries no analyzer
// flags, these values survive unitchecker's own flag parse (which only
// overrides flags present on the command line). Unknown analyzer/flag names are
// an error so a typo fails loudly rather than being silently dropped.
func applyGovetFlags(selected []*analysis.Analyzer, s govetSettings) error {
	if len(s.Settings) == 0 {
		return nil
	}
	byName := make(map[string]*analysis.Analyzer, len(selected))
	for _, a := range selected {
		byName[a.Name] = a
	}
	// Deterministic iteration for stable error messages.
	names := make([]string, 0, len(s.Settings))
	for n := range s.Settings {
		names = append(names, n)
	}
	sort.Strings(names)
	for _, name := range names {
		a, ok := byName[name]
		if !ok {
			return fmt.Errorf("settings.govet.settings: analyzer %q is not enabled", name)
		}
		flags := s.Settings[name]
		fnames := make([]string, 0, len(flags))
		for f := range flags {
			fnames = append(fnames, f)
		}
		sort.Strings(fnames)
		for _, f := range fnames {
			if a.Flags.Lookup(f) == nil {
				return fmt.Errorf("settings.govet.settings.%s: unknown flag %q", name, f)
			}
			if err := a.Flags.Set(f, coerceFlag(flags[f])); err != nil {
				return fmt.Errorf("settings.govet.settings.%s.%s: %w", name, f, err)
			}
		}
	}
	return nil
}

// coerceFlag renders a YAML value into the string form a go/analysis flag
// expects: lists join on comma (e.g. printf.funcs), bools to "true"/"false",
// scalars via fmt.
func coerceFlag(v any) string {
	switch t := v.(type) {
	case []any:
		parts := make([]string, 0, len(t))
		for _, e := range t {
			parts = append(parts, coerceFlag(e))
		}
		return strings.Join(parts, ",")
	case bool:
		return strconv.FormatBool(t)
	case string:
		return t
	default:
		return fmt.Sprint(t)
	}
}

// applyHonnefConfig overrides honnef's `config.Analyzer.Run` in place so its
// checks (ST1001/ST1003/ST1013 via config.For) see initialisms + whitelists
// from `.golangci.yml` instead of a `staticcheck.conf` on disk. Fields left
// unset in the config fall back to honnef's defaults so we never drop the
// standard initialisms. Only installed when at least one field is set, so the
// default (file-based, or DefaultConfig) behavior is otherwise untouched.
//
// `checks` is NOT applied here — honnef checks do not self-disable on
// Config.Checks (only honnef's own runner does); check selection is handled by
// filterChecks narrowing the analyzer set.
func applyHonnefConfig(s linterSettings) {
	merged := config.DefaultConfig
	set := false
	// stylecheck owns these knobs in golangci; accept them from any family that
	// sets one, last-writer-wins in family order.
	for _, hs := range []honnefSettings{s.Staticcheck, s.Gosimple, s.Stylecheck} {
		if len(hs.Initialisms) > 0 {
			merged.Initialisms = hs.Initialisms
			set = true
		}
		if len(hs.DotImportWhitelist) > 0 {
			merged.DotImportWhitelist = hs.DotImportWhitelist
			set = true
		}
		if len(hs.HTTPStatusCodeWhitelist) > 0 {
			merged.HTTPStatusCodeWhitelist = hs.HTTPStatusCodeWhitelist
			set = true
		}
	}
	if !set {
		return
	}
	config.Analyzer.Run = func(*analysis.Pass) (any, error) {
		cfg := merged
		return &cfg, nil
	}
}
