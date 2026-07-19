package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestScanNolint(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "a.go")
	src := `package p

func F(b bool) {
	_ = b == true //nolint:gosimple
	_ = b == false
	//nolint:staticcheck
	_ = 1
	_ = 2 //nolint
}
`
	if err := os.WriteFile(file, []byte(src), 0o644); err != nil {
		t.Fatal(err)
	}
	idx := scanNolint(file)

	// Trailing directive covers its own line (4), scoped to gosimple.
	if !idx[4]["gosimple"] {
		t.Errorf("line 4 should be nolint:gosimple, got %v", idx[4])
	}
	// Line 5 has no directive.
	if idx[5] != nil {
		t.Errorf("line 5 should have no directive, got %v", idx[5])
	}
	// Own-line directive (6) covers 6 and the next line (7), scoped to staticcheck.
	if !idx[6]["staticcheck"] || !idx[7]["staticcheck"] {
		t.Errorf("lines 6,7 should be nolint:staticcheck, got %v / %v", idx[6], idx[7])
	}
	// Bare //nolint covers all linters via "*".
	if !idx[8]["*"] {
		t.Errorf("line 8 bare nolint should set *, got %v", idx[8])
	}
}

func TestScanNolintWordBoundary(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "a.go")
	// `//nolintlint` and `//nolinter` share the `nolint` prefix but are NOT
	// nolint directives: they must not suppress anything (and in particular must
	// not be read as a bare `//nolint` that suppresses every linter).
	src := `package p

func F(b bool) {
	_ = b == true //nolintlint
	_ = b == false //nolinter:staticcheck
	_ = b == b //nolint:gosimple
}
`
	if err := os.WriteFile(file, []byte(src), 0o644); err != nil {
		t.Fatal(err)
	}
	idx := scanNolint(file)

	if idx[4] != nil {
		t.Errorf("line 4 //nolintlint must not be a directive, got %v", idx[4])
	}
	if idx[5] != nil {
		t.Errorf("line 5 //nolinter must not be a directive, got %v", idx[5])
	}
	// The genuine directive still matches.
	if !idx[6]["gosimple"] {
		t.Errorf("line 6 //nolint:gosimple should match, got %v", idx[6])
	}
}

func TestScanNolintAllIsWildcard(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "a.go")
	// `//nolint:all` is golangci's wildcard — it must suppress every linter on
	// the line, exactly like a bare `//nolint`.
	if err := os.WriteFile(file, []byte("package p\n_ = 0 //nolint:all\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	idx := scanNolint(file)
	if !idx[2]["*"] {
		t.Errorf("//nolint:all should set the * wildcard, got %v", idx[2])
	}

	s, err := newSuppressor(golangciConfig{})
	if err != nil {
		t.Fatal(err)
	}
	if !s.suppressed("gosimple", file, 2, "msg") {
		t.Error("//nolint:all must suppress gosimple")
	}
	if !s.suppressed("staticcheck", file, 2, "msg") {
		t.Error("//nolint:all must suppress staticcheck")
	}
}

func TestSuppressedNolintScoping(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "a.go")
	if err := os.WriteFile(file, []byte("package p\n_ = 0 //nolint:gosimple\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	s, err := newSuppressor(golangciConfig{})
	if err != nil {
		t.Fatal(err)
	}
	if !s.suppressed("gosimple", file, 2, "msg") {
		t.Error("gosimple on line 2 should be suppressed")
	}
	if s.suppressed("staticcheck", file, 2, "msg") {
		t.Error("staticcheck must NOT be suppressed by //nolint:gosimple")
	}
	if s.suppressed("gosimple", file, 3, "msg") {
		t.Error("line 3 must not be suppressed")
	}
}

func TestSuppressedExcludeRules(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Rules = []excludeRuleYAML{
		// v2 folds gosimple into staticcheck, so the rule scopes to staticcheck.
		{Linters: []string{"staticcheck"}, Text: "omit comparison"},
		{Path: `_test\.go$`},
	}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}

	// Text rule, scoped to staticcheck.
	if !s.suppressed("staticcheck", "a.go", 1, "should omit comparison to bool") {
		t.Error("matching text+linter should suppress")
	}
	if s.suppressed("govet", "a.go", 1, "should omit comparison to bool") {
		t.Error("text rule scoped to staticcheck must not suppress govet")
	}
	if s.suppressed("staticcheck", "a.go", 1, "unrelated message") {
		t.Error("non-matching text must not suppress")
	}
	// Path rule, any linter.
	if !s.suppressed("anylinter", "pkg/a_test.go", 1, "anything") {
		t.Error("path rule should suppress in _test.go")
	}
	if s.suppressed("anylinter", "pkg/a.go", 1, "anything") {
		t.Error("path rule must not suppress non-test file")
	}
}

func TestMatchTextPrefixesHonnefWithCode(t *testing.T) {
	// golangci matches staticcheck text rules against `CODE: message`.
	if got := matchText("staticcheck", "SA1019", "nrn.ResourceSite is deprecated"); got != "SA1019: nrn.ResourceSite is deprecated" {
		t.Errorf("staticcheck text should be code-prefixed, got %q", got)
	}
	// go vet messages are matched unprefixed.
	if got := matchText("govet", "printf", "wrong arg"); got != "wrong arg" {
		t.Errorf("govet text must be unprefixed, got %q", got)
	}
}

func TestSuppressedStaticcheckTextRuleWithCodePrefix(t *testing.T) {
	// A rule copied from a golangci config, including the `SA1019:` prefix.
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable"
	cfg.Linters.Exclusions.Rules = []excludeRuleYAML{
		{
			Path: `(.+)\.go$`,
			Text: "SA1019: nrn.ResourceSite is deprecated: Use ResourceClient or ResourceGateway",
		},
	}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	// heph-govet feeds the code-prefixed form to the suppressor (see withFilter).
	msg := matchText("staticcheck", "SA1019",
		"nrn.ResourceSite is deprecated: Use ResourceClient or ResourceGateway")
	if !s.suppressed("staticcheck", "workers/auditupdown/process.go", 117, msg) {
		t.Errorf("golangci-format SA1019 text rule should suppress; matched against %q", msg)
	}
}

func TestSuppressedGeneratedFile(t *testing.T) {
	dir := t.TempDir()
	gen := filepath.Join(dir, "gql_generated.go")
	if err := os.WriteFile(gen,
		[]byte("// Code generated by gqlgen. DO NOT EDIT.\n\npackage p\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	hand := filepath.Join(dir, "hand.go")
	if err := os.WriteFile(hand, []byte("package p\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	// Default (lax): generated file suppressed, hand-written not.
	s, err := newSuppressor(golangciConfig{})
	if err != nil {
		t.Fatal(err)
	}
	if !s.suppressed("staticcheck", gen, 1, "anything") {
		t.Error("generated file must be excluded by default (lax)")
	}
	if s.suppressed("staticcheck", hand, 1, "anything") {
		t.Error("hand-written file must not be excluded")
	}

	// disable: even a generated file is linted.
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable"
	s2, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if s2.suppressed("staticcheck", gen, 1, "anything") {
		t.Error("generated:disable must not exclude generated files")
	}
}

func TestSuppressedGeneratedStrictVsLax(t *testing.T) {
	dir := t.TempDir()
	// A "generated by" header is lax-only; strict wants the Go-convention line.
	lax := filepath.Join(dir, "a.go")
	if err := os.WriteFile(lax, []byte("// Autogenerated by thrift. Do not modify.\n\npackage p\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	var strictCfg golangciConfig
	strictCfg.Linters.Exclusions.Generated = "strict"
	strict, err := newSuppressor(strictCfg)
	if err != nil {
		t.Fatal(err)
	}
	if strict.suppressed("staticcheck", lax, 1, "x") {
		t.Error("strict mode must not treat a lax-only header as generated")
	}
	laxS, err := newSuppressor(golangciConfig{})
	if err != nil {
		t.Fatal(err)
	}
	if !laxS.suppressed("staticcheck", lax, 1, "x") {
		t.Error("lax mode must treat 'Autogenerated by' as generated")
	}
}

func TestSuppressedPaths(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable" // isolate path behavior
	cfg.Linters.Exclusions.Paths = []string{`internal/gen/.*`, `.*/gql_generated\.go`}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if !s.suppressed("govet", "/ws/pkg/internal/gen/x.go", 1, "m") {
		t.Error("path under internal/gen must be excluded")
	}
	if !s.suppressed("govet", "/ws/svc/gql_generated.go", 1, "m") {
		t.Error("gql_generated.go must be excluded")
	}
	if s.suppressed("govet", "/ws/pkg/real.go", 1, "m") {
		t.Error("unrelated file must not be excluded")
	}
}

func TestSuppressedPathsExcept(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable"
	cfg.Linters.Exclusions.PathsExcept = []string{`/lint/.*`}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	// Only files under /lint/ are linted; everything else is excluded.
	if s.suppressed("govet", "/ws/lint/a.go", 1, "m") {
		t.Error("file matching paths-except must be linted")
	}
	if !s.suppressed("govet", "/ws/other/a.go", 1, "m") {
		t.Error("file not matching paths-except must be excluded")
	}
}

func TestSuppressedRulePathExcept(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable"
	// Exclude "foo" everywhere except in _test.go files.
	cfg.Linters.Exclusions.Rules = []excludeRuleYAML{
		{Text: "foo", PathExcept: `_test\.go$`},
	}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if !s.suppressed("govet", "a.go", 1, "foo bar") {
		t.Error("rule must suppress in a non-excepted path")
	}
	if s.suppressed("govet", "a_test.go", 1, "foo bar") {
		t.Error("path-except must exempt _test.go from the rule")
	}
}

func TestPresetsExpandAndSuppress(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Generated = "disable"
	cfg.Linters.Exclusions.Presets = []string{"comments", "legacy"}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	// comments (EXC0011, staticcheck) drops the package-comment message.
	if !s.suppressed("staticcheck", "a.go", 1,
		"at least one file in a package should have a package comment") {
		t.Error("comments preset must exclude the package-comment finding")
	}
	// legacy (EXC0004, govet) drops the unsafe.Pointer misuse message.
	if !s.suppressed("govet", "a.go", 1, "possible misuse of unsafe.Pointer") {
		t.Error("legacy preset must exclude the unsafe.Pointer finding")
	}
	// An unrelated staticcheck finding is untouched.
	if s.suppressed("staticcheck", "a.go", 1, "SA4006: never used") {
		t.Error("preset must not over-suppress")
	}
}

func TestPresetsUnknownFails(t *testing.T) {
	var cfg golangciConfig
	cfg.Linters.Exclusions.Presets = []string{"nonsense"}
	if _, err := newSuppressor(cfg); err == nil {
		t.Fatal("unknown preset must fail loudly")
	}
}
