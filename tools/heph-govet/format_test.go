package main

import (
	"bytes"
	"testing"
)

func TestEnabledFormattersDefaultsToGofmt(t *testing.T) {
	got := enabledFormatters(nil)
	if len(got) != 1 || got[0] != "gofmt" {
		t.Fatalf("default should be [gofmt], got %v", got)
	}
}

func TestFormatBytesGofmtReformats(t *testing.T) {
	// Badly spaced but valid Go.
	src := []byte("package p\nfunc  f( ){\n}\n")
	out, err := formatBytes("f.go", src, []string{"gofmt"}, formattersConfig{})
	if err != nil {
		t.Fatalf("format: %v", err)
	}
	if bytes.Equal(src, out) {
		t.Fatal("gofmt should have reformatted")
	}
	// Idempotent: formatting the formatted output is a no-op.
	out2, err := formatBytes("f.go", out, []string{"gofmt"}, formattersConfig{})
	if err != nil {
		t.Fatalf("format2: %v", err)
	}
	if !bytes.Equal(out, out2) {
		t.Fatal("gofmt must be idempotent")
	}
}

func TestFormatBytesGofmtSimplify(t *testing.T) {
	// `s[a:len(s)]` simplifies to `s[a:]` only under -s.
	src := []byte("package p\n\nfunc f(s []int, a int) []int { return s[a:len(s)] }\n")
	plain, err := formatBytes("f.go", src, []string{"gofmt"}, formattersConfig{})
	if err != nil {
		t.Fatalf("plain: %v", err)
	}
	if !bytes.Contains(plain, []byte("s[a:len(s)]")) {
		t.Fatalf("without simplify the slice expr should be untouched: %s", plain)
	}

	var fc formattersConfig
	fc.Settings.Gofmt.Simplify = true
	simplified, err := formatBytes("f.go", src, []string{"gofmt"}, fc)
	if err != nil {
		t.Fatalf("simplify: %v", err)
	}
	if !bytes.Contains(simplified, []byte("s[a:]")) {
		t.Fatalf("simplify should rewrite s[a:len(s)] -> s[a:]: %s", simplified)
	}
}

func TestFormatBytesGofumpt(t *testing.T) {
	// gofumpt removes the blank line at the start of a block — a rule beyond gofmt.
	src := []byte("package p\n\nfunc f() {\n\n\tx := 1\n\t_ = x\n}\n")
	gofmted, err := formatBytes("f.go", src, []string{"gofmt"}, formattersConfig{})
	if err != nil {
		t.Fatalf("gofmt: %v", err)
	}
	if !bytes.Contains(gofmted, []byte("{\n\n")) {
		t.Fatal("precondition: gofmt keeps the leading blank line")
	}
	fumpted, err := formatBytes("f.go", src, []string{"gofumpt"}, formattersConfig{})
	if err != nil {
		t.Fatalf("gofumpt: %v", err)
	}
	if bytes.Contains(fumpted, []byte("{\n\n")) {
		t.Fatalf("gofumpt should drop the leading blank line: %q", fumpted)
	}
}

func TestFormatBytesGoimportsSortsExisting(t *testing.T) {
	// Out-of-order, still-used imports get sorted; FormatOnly means none added.
	src := []byte("package p\n\nimport (\n\t\"os\"\n\t\"fmt\"\n)\n\nfunc f() { fmt.Fprintln(os.Stdout) }\n")
	out, err := formatBytes("f.go", src, []string{"goimports"}, formattersConfig{})
	if err != nil {
		t.Fatalf("goimports: %v", err)
	}
	fmtIdx := bytes.Index(out, []byte(`"fmt"`))
	osIdx := bytes.Index(out, []byte(`"os"`))
	if fmtIdx == -1 || osIdx == -1 || fmtIdx > osIdx {
		t.Fatalf("goimports should sort fmt before os: %s", out)
	}
}

func TestFormatBytesNoopWhenAlreadyFormatted(t *testing.T) {
	src := []byte("package p\n")
	out, err := formatBytes("f.go", src, []string{"gofmt", "gofumpt", "goimports"}, formattersConfig{})
	if err != nil {
		t.Fatalf("format: %v", err)
	}
	if !bytes.Equal(src, out) {
		t.Fatalf("already-formatted file must be unchanged, got %q", out)
	}
}
