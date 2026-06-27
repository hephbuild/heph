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
	cfg.Linters.Exclusions.Rules = []struct {
		Linters []string `yaml:"linters"`
		Path    string   `yaml:"path"`
		Text    string   `yaml:"text"`
	}{
		{Linters: []string{"gosimple"}, Text: "omit comparison"},
		{Path: `_test\.go$`},
	}
	s, err := newSuppressor(cfg)
	if err != nil {
		t.Fatal(err)
	}

	// Text rule, scoped to gosimple.
	if !s.suppressed("gosimple", "a.go", 1, "should omit comparison to bool") {
		t.Error("matching text+linter should suppress")
	}
	if s.suppressed("staticcheck", "a.go", 1, "should omit comparison to bool") {
		t.Error("text rule scoped to gosimple must not suppress staticcheck")
	}
	if s.suppressed("gosimple", "a.go", 1, "unrelated message") {
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
