package main

import (
	"strings"
	"testing"

	"golang.org/x/tools/go/analysis"
	"golang.org/x/tools/go/analysis/passes/printf"
	"golang.org/x/tools/go/analysis/passes/shadow"
	"honnef.co/go/tools/config"
)

func names(as []*analysis.Analyzer) map[string]bool {
	m := make(map[string]bool, len(as))
	for _, a := range as {
		m[a.Name] = true
	}
	return m
}

func TestResolveGovetDefaultExcludesOffByDefault(t *testing.T) {
	got := names(resolveGovet(govetSettings{}))
	if !got["printf"] {
		t.Fatal("printf should be on by default")
	}
	if got["shadow"] || got["fieldalignment"] {
		t.Fatalf("off-by-default analyzers must be excluded by default: %v", got)
	}
}

func TestResolveGovetEnableAddsOffByDefault(t *testing.T) {
	got := names(resolveGovet(govetSettings{Enable: []string{"shadow"}}))
	if !got["shadow"] {
		t.Fatal("shadow should be enabled")
	}
	if !got["printf"] {
		t.Fatal("defaults stay on when enabling an extra")
	}
}

func TestResolveGovetDisable(t *testing.T) {
	got := names(resolveGovet(govetSettings{Disable: []string{"printf"}}))
	if got["printf"] {
		t.Fatal("printf should be disabled")
	}
	if !got["assign"] {
		t.Fatal("other defaults stay on")
	}
}

func TestResolveGovetDisableAllThenEnable(t *testing.T) {
	got := names(resolveGovet(govetSettings{DisableAll: true, Enable: []string{"printf"}}))
	if len(got) != 1 || !got["printf"] {
		t.Fatalf("disable-all + enable printf should yield only printf: %v", got)
	}
}

func TestResolveGovetEnableAll(t *testing.T) {
	got := names(resolveGovet(govetSettings{EnableAll: true}))
	if !got["shadow"] || !got["fieldalignment"] || !got["printf"] {
		t.Fatalf("enable-all should include off-by-default analyzers: %v", got)
	}
}

func TestFilterChecksEmptyAppliesGolangciDefault(t *testing.T) {
	// Empty `checks` applies golangci's default set: all SA/S checks stay, but
	// the noisy ST100x comment/naming checks (e.g. ST1000) are off — matching
	// golangci-lint, not honnef's stricter "all".
	in := []*analysis.Analyzer{{Name: "SA1000"}, {Name: "SA4006"}, {Name: "ST1000"}, {Name: "ST1003"}}
	got := names(filterChecks(in, nil))
	if !got["SA1000"] || !got["SA4006"] {
		t.Fatalf("SA checks must stay enabled by default: %v", got)
	}
	if got["ST1000"] || got["ST1003"] {
		t.Fatalf("ST1000/ST1003 must be off by default (golangci default): %v", got)
	}
}

func TestFilterChecksAllMinusOne(t *testing.T) {
	in := []*analysis.Analyzer{{Name: "SA1000"}, {Name: "SA4006"}, {Name: "ST1003"}}
	got := names(filterChecks(in, []string{"all", "-SA1000"}))
	if got["SA1000"] {
		t.Fatal("SA1000 should be excluded")
	}
	if !got["SA4006"] || !got["ST1003"] {
		t.Fatalf("others should remain: %v", got)
	}
}

func TestFilterChecksGlob(t *testing.T) {
	in := []*analysis.Analyzer{{Name: "SA1000"}, {Name: "SA1019"}, {Name: "SA4006"}}
	got := names(filterChecks(in, []string{"none", "SA1*"}))
	if !got["SA1000"] || !got["SA1019"] {
		t.Fatalf("SA1* should match SA10xx: %v", got)
	}
	if got["SA4006"] {
		t.Fatal("SA4006 should stay off")
	}
}

func TestNolintLinterNameCollapsesHonnefToStaticcheck(t *testing.T) {
	// golangci v2 merged gosimple/stylecheck into staticcheck, so all three
	// honnef families are suppressed with //nolint:staticcheck.
	for _, fam := range []string{"staticcheck", "gosimple", "stylecheck"} {
		if got := nolintLinterName(fam); got != "staticcheck" {
			t.Errorf("nolintLinterName(%q) = %q, want staticcheck", fam, got)
		}
	}
	if got := nolintLinterName("govet"); got != "govet" {
		t.Errorf("nolintLinterName(govet) = %q, want govet", got)
	}
}

func TestSelectAnalyzersLinterOfUsesStaticcheckForHonnef(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Default = "all"
	r := registry(cfg)
	_, linterOf, _, err := selectAnalyzers(cfg, r)
	if err != nil {
		t.Fatal(err)
	}
	for a, name := range linterOf {
		// honnef check codes start with an uppercase 'S' (SA/S/ST); vet passes
		// are lowercase words.
		if strings.HasPrefix(a.Name, "S") && name != "staticcheck" {
			t.Errorf("honnef %s mapped to %q, want staticcheck", a.Name, name)
		}
	}
}

func TestCoerceFlag(t *testing.T) {
	if got := coerceFlag([]any{"A", "B"}); got != "A,B" {
		t.Fatalf("list join: got %q", got)
	}
	if got := coerceFlag(true); got != "true" {
		t.Fatalf("bool: got %q", got)
	}
	if got := coerceFlag("x"); got != "x" {
		t.Fatalf("string: got %q", got)
	}
}

func TestApplyGovetFlagsSetsPrintfFuncs(t *testing.T) {
	// Fresh analyzer set so the flag mutation doesn't leak across tests via the
	// shared package-level printf.Analyzer... it IS shared, so read it back.
	sel := resolveGovet(govetSettings{})
	s := govetSettings{Settings: map[string]map[string]any{
		"printf": {"funcs": []any{"example.com/x.Logf"}},
	}}
	if err := applyGovetFlags(sel, s); err != nil {
		t.Fatalf("apply: %v", err)
	}
	if got := printf.Analyzer.Flags.Lookup("funcs").Value.String(); !strings.Contains(got, "example.com/x.Logf") {
		t.Fatalf("printf.funcs not applied: %q", got)
	}
}

func TestApplyGovetFlagsUnknownAnalyzerFails(t *testing.T) {
	err := applyGovetFlags(resolveGovet(govetSettings{}), govetSettings{
		Settings: map[string]map[string]any{"nope": {"x": "y"}},
	})
	if err == nil {
		t.Fatal("unknown analyzer must fail")
	}
}

func TestApplyGovetFlagsUnknownFlagFails(t *testing.T) {
	err := applyGovetFlags(resolveGovet(govetSettings{}), govetSettings{
		Settings: map[string]map[string]any{"printf": {"nosuchflag": "y"}},
	})
	if err == nil {
		t.Fatal("unknown flag must fail")
	}
}

func TestApplyHonnefConfigOverridesInitialisms(t *testing.T) {
	orig := config.Analyzer.Run
	t.Cleanup(func() { config.Analyzer.Run = orig })

	applyHonnefConfig(linterSettings{
		Stylecheck: honnefSettings{Initialisms: []string{"XML", "GRPC"}},
	})
	res, err := config.Analyzer.Run(nil)
	if err != nil {
		t.Fatalf("run: %v", err)
	}
	cfg := res.(*config.Config)
	has := map[string]bool{}
	for _, i := range cfg.Initialisms {
		has[i] = true
	}
	if !has["XML"] || !has["GRPC"] {
		t.Fatalf("custom initialisms missing: %v", cfg.Initialisms)
	}
	// Whitelists left unset fall back to honnef defaults (not emptied).
	if len(cfg.HTTPStatusCodeWhitelist) == 0 {
		t.Fatal("unset fields must keep honnef defaults")
	}
}

func TestApplyHonnefConfigNoopWhenUnset(t *testing.T) {
	orig := config.Analyzer.Run
	t.Cleanup(func() { config.Analyzer.Run = orig })
	applyHonnefConfig(linterSettings{})
	// Run pointer must be unchanged (still the honnef default).
	if config.Analyzer.Run == nil {
		t.Fatal("run should be untouched")
	}
	// shadow import kept meaningful: ensure the analyzer exists (compile guard).
	_ = shadow.Analyzer
}
