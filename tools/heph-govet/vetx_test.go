package main

import (
	"bytes"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"golang.org/x/tools/go/analysis/unitchecker"
)

// reexecEnv makes the test binary run heph-govet's main instead of the tests, so
// a test can drive the real binary (unitchecker calls os.Exit) as a subprocess.
const reexecEnv = "HEPH_GOVET_TEST_REEXEC"

func TestMain(m *testing.M) {
	if os.Getenv(reexecEnv) != "" {
		main()
		return
	}
	os.Exit(m.Run())
}

// stdExports maps each package in the closure of pkgs to its compiled export data,
// the way the heph go_lint driver stages `.a` archives into PackageFile.
func stdExports(t *testing.T, pkgs ...string) map[string]string {
	t.Helper()
	args := append([]string{"list", "-export", "-deps", "-f", "{{.ImportPath}}={{.Export}}"}, pkgs...)
	out, err := exec.Command("go", args...).Output()
	if err != nil {
		t.Fatalf("go list -export: %v", err)
	}
	m := map[string]string{}
	for line := range strings.Lines(string(out)) {
		path, file, ok := strings.Cut(strings.TrimSpace(line), "=")
		if ok && file != "" {
			m[path] = file
		}
	}
	return m
}

// runUnit writes a unitchecker cfg for one package and runs heph-govet over it.
// It returns the vetx file it wrote when vetx is nil (a leaf), and the JSON
// report otherwise.
func runUnit(t *testing.T, dir, importPath, src string, imports []string, files, vetx map[string]string) string {
	t.Helper()
	pkgDir := filepath.Join(dir, importPath)
	if err := os.MkdirAll(pkgDir, 0o755); err != nil {
		t.Fatal(err)
	}
	goFile := filepath.Join(pkgDir, "p.go")
	if err := os.WriteFile(goFile, []byte(src), 0o644); err != nil {
		t.Fatal(err)
	}
	importMap := map[string]string{}
	for _, ip := range imports {
		importMap[ip] = ip
	}
	out := filepath.Join(pkgDir, "lint.facts")
	cfg := unitchecker.Config{
		Compiler:    "gc",
		Dir:         pkgDir,
		ImportPath:  importPath,
		GoFiles:     []string{goFile},
		ImportMap:   importMap,
		PackageFile: files,
		PackageVetx: vetx,
		VetxOutput:  out,
	}
	data, err := json.Marshal(cfg)
	if err != nil {
		t.Fatal(err)
	}
	cfgPath := filepath.Join(pkgDir, "unitchecker.cfg")
	if err := os.WriteFile(cfgPath, data, 0o644); err != nil {
		t.Fatal(err)
	}
	golangci := filepath.Join(dir, ".golangci.yml")
	if err := os.WriteFile(golangci, []byte("version: \"2\"\nlinters:\n  default: none\n  enable: [govet]\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	self, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(self, "-json", cfgPath)
	cmd.Env = append(os.Environ(), reexecEnv+"=1", configEnvVar+"="+golangci)
	var stderr, stdout bytes.Buffer
	cmd.Stderr, cmd.Stdout = &stderr, &stdout
	if err := cmd.Run(); err != nil {
		t.Fatalf("heph-govet on %s: %v\nstderr:\n%s\nstdout:\n%s", importPath, err, stderr.String(), stdout.String())
	}
	// -json exits 0 even on findings; a type-check failure is reported in the JSON.
	if strings.Contains(stdout.String(), `"error"`) {
		t.Fatalf("heph-govet on %s reported an error:\n%s", importPath, stdout.String())
	}
	if _, err := os.Stat(out); err != nil {
		t.Fatalf("no facts written for %s: %v", importPath, err)
	}
	if vetx == nil {
		return out
	}
	return stdout.String()
}

// The heph go_lint driver stages a vetx (facts) file only for packages heph lints
// itself — never the standard library. x/tools ≥ v0.50 reads an import's *types*
// from its vetx file, not its archive, so a package importing net/http failed with
// `could not import net/http (no package vetx file for "net/http")`. heph-govet
// must stand in a vetx file for every import that has only an archive.
//
// Two units, like a real dependent: `a` (imports fmt) has its facts produced
// here, and `b` imports both `a` (with facts) and net/http (archive only).
func TestImportWithoutVetxFile(t *testing.T) {
	if _, err := exec.LookPath("go"); err != nil {
		t.Skip("no go on PATH")
	}
	dir := t.TempDir()
	files := stdExports(t, "fmt", "net/http")

	aVetx := runUnit(t, dir, "example.com/a",
		"package a\n\nimport \"fmt\"\n\nfunc Logf(format string, args ...any) { fmt.Printf(format, args...) }\n",
		[]string{"fmt"}, files, nil)

	// b type-checks against a's types, which come only from a's vetx file.
	bFiles := map[string]string{"example.com/a": filepath.Join(dir, "unused.a")}
	for k, v := range files {
		bFiles[k] = v
	}
	report := runUnit(t, dir, "example.com/b",
		"package b\n\nimport (\n\t\"net/http\"\n\n\t\"example.com/a\"\n)\n\nfunc F() { a.Logf(\"%d\", http.MethodGet) }\n",
		[]string{"net/http", "example.com/a"}, bFiles,
		map[string]string{"example.com/a": aVetx})

	// printf only knows a.Logf is a wrapper from a's facts: the finding proves the
	// analysis ran and a's real vetx file was still read alongside the synthesized one.
	if !strings.Contains(report, "printf") || !strings.Contains(report, "Logf") {
		t.Fatalf("expected printf to flag a.Logf(%%d, string) via a's facts, got:\n%s", report)
	}
}
