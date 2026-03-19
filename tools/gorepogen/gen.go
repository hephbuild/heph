// gorepogen generates a synthetic Go repository for stress-testing build systems.
//
// Features:
//   - Deterministic output via a seed flag
//   - Programmable nesting (packages at different directory depths)
//   - Third-party dependency usage including k8s.io/apimachinery
//   - Intra-repo imports (DAG-ordered to avoid cycles)
//   - Each package randomly gets: test only, xtest only, both, or no tests
//   - ~20% of packages embed static asset files via //go:embed
//   - Import name collisions are resolved with auto-aliasing
//
// Usage:
//
//	go run ./tools/gorepogen \
//	  -seed 42 \
//	  -out /tmp/myrepo \
//	  -module example.com/myrepo \
//	  -pkgs 30 \
//	  -max-depth 4
package main

import (
	"flag"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"text/template"
)

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

type Config struct {
	Seed     int64
	Out      string
	Module   string
	NumPkgs  int
	MaxDepth int
}

// ---------------------------------------------------------------------------
// Third-party dependencies catalogue
// ---------------------------------------------------------------------------

// ThirdPartyDep describes one importable package from outside the module.
//
// Symbol is a plain Go expression that uses the package, written with the
// package's natural identifier (last path element, or ImportAlias when set).
// The generator adds "_ = " in front automatically and rewrites the identifier
// when it needs to assign an alias to resolve a name collision.
//
// Avoid expressions that return multiple values; use a function/method value
// reference (e.g. "uuid.NewString") or a single-return call instead.
type ThirdPartyDep struct {
	ImportPath  string
	ImportAlias string // explicit alias; empty = use last path element
	Symbol      string // plain expression, e.g. `uuid.New()`
}

// defaultIdent returns the identifier Go code uses to qualify this package
// (the ImportAlias when set, otherwise the last segment of ImportPath).
func (d ThirdPartyDep) defaultIdent() string {
	if d.ImportAlias != "" {
		return d.ImportAlias
	}
	parts := strings.Split(d.ImportPath, "/")
	return parts[len(parts)-1]
}

var thirdPartyDeps = []ThirdPartyDep{
	// --- small, focused libraries ---
	{ImportPath: "github.com/google/uuid", Symbol: `uuid.New()`},
	{ImportPath: "github.com/zeebo/xxh3", Symbol: `xxh3.HashString("x")`},
	// method value – avoids calling a zero-receiver method, returns func() error
	{ImportPath: "golang.org/x/sync/errgroup", Symbol: `(*errgroup.Group)(nil).Wait`},
	{ImportPath: "github.com/spf13/pflag", Symbol: `pflag.String`},
	{ImportPath: "golang.org/x/text/unicode/norm", Symbol: `norm.NFC.String("x")`},
	// function value – avoids multi-return without calling the function
	{ImportPath: "github.com/pkg/xattr", Symbol: `xattr.List`},
	// --- k8s.io/apimachinery sub-packages ---
	{ImportPath: "k8s.io/apimachinery/pkg/types", Symbol: `types.UID("x")`},
	{ImportPath: "k8s.io/apimachinery/pkg/util/sets", Symbol: `sets.New[string]("a")`},
	{ImportPath: "k8s.io/apimachinery/pkg/labels", Symbol: `labels.Everything()`},
	{
		ImportPath:  "k8s.io/apimachinery/pkg/util/errors",
		ImportAlias: "utilerrors",
		Symbol:      `utilerrors.NewAggregate(nil)`,
	},
	{ImportPath: "k8s.io/apimachinery/pkg/runtime/schema", Symbol: `schema.GroupVersionResource{}`},
	{ImportPath: "k8s.io/apimachinery/pkg/api/validation", Symbol: `validation.ValidateNamespaceName("x", false)`},
}

// thirdPartyModules maps each module root to its pinned version.
// Only modules actually used appear in the generated go.mod.
var thirdPartyModules = map[string]string{
	"github.com/google/uuid": "v1.6.0",
	"github.com/zeebo/xxh3":  "v1.0.2",
	"golang.org/x/sync":      "v0.19.0",
	"github.com/spf13/pflag": "v1.0.10",
	"golang.org/x/text":      "v0.34.0",
	"github.com/pkg/xattr":   "v0.4.12",
	"k8s.io/apimachinery":    "v0.32.1",
}

// moduleForImport returns the go module root for an import path.
func moduleForImport(imp string) string {
	for mod := range thirdPartyModules {
		if imp == mod || strings.HasPrefix(imp, mod+"/") {
			return mod
		}
	}
	panic("unknown module for import: " + imp)
}

// ---------------------------------------------------------------------------
// Package descriptor
// ---------------------------------------------------------------------------

// TestKind describes what test files a package has.
type TestKind int

const (
	TestNone  TestKind = iota // no test files
	TestOnly                  // _test.go with `package foo`
	XTestOnly                 // _test.go with `package foo_test`
	TestBoth                  // both of the above in separate files
)

type Package struct {
	Name               string
	RelDir             string
	ImportPath         string
	InternalDeps       []int // indices into packages slice (always < own index)
	ThirdPartyDeps     []ThirdPartyDep
	TestKind           TestKind
	ThirdPartyTestDeps []ThirdPartyDep
	EmbedFiles         bool
}

// ---------------------------------------------------------------------------
// Randomisation helpers
// ---------------------------------------------------------------------------

var nameSegments = []string{
	"alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
	"iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi",
	"rho", "sigma", "tau", "upsilon", "phi", "chi", "psi", "omega",
	"core", "base", "util", "pkg", "lib", "api", "svc", "io",
	"net", "sys", "cmd", "data", "store", "cache", "sync", "trace",
}

func buildRelDir(r *rand.Rand, maxDepth int, usedPaths map[string]bool) string {
	depth := 1 + r.Intn(maxDepth)
	for attempt := 0; attempt < 100; attempt++ {
		parts := make([]string, depth)
		for i := range parts {
			parts[i] = nameSegments[r.Intn(len(nameSegments))]
		}
		rel := strings.Join(parts, "/")
		if !usedPaths[rel] {
			usedPaths[rel] = true
			return rel
		}
	}
	base := fmt.Sprintf("pkg%d", len(usedPaths))
	usedPaths[base] = true
	return base
}

func pickThirdPartyDeps(r *rand.Rand) []ThirdPartyDep {
	n := r.Intn(3) // 0, 1, or 2
	if n == 0 {
		return nil
	}
	perm := r.Perm(len(thirdPartyDeps))
	deps := make([]ThirdPartyDep, 0, n)
	for i := 0; i < n; i++ {
		deps = append(deps, thirdPartyDeps[perm[i]])
	}
	return deps
}

func pickInternalDeps(r *rand.Rand, idx int) []int {
	if idx == 0 {
		return nil
	}
	maxDeps := 3
	if idx < maxDeps {
		maxDeps = idx
	}
	n := r.Intn(maxDeps + 1)
	if n == 0 {
		return nil
	}
	perm := r.Perm(idx)
	deps := make([]int, n)
	for i := range deps {
		deps[i] = perm[i]
	}
	sort.Ints(deps)
	return deps
}

func pickTestKind(r *rand.Rand) TestKind {
	v := r.Intn(100)
	switch {
	case v < 15:
		return TestNone
	case v < 50:
		return TestOnly
	case v < 85:
		return XTestOnly
	default:
		return TestBoth
	}
}

func pickEmbedFiles(r *rand.Rand) bool {
	return r.Intn(100) < 20
}

// ---------------------------------------------------------------------------
// Import alias resolution
//
// When multiple imports share the same default identifier (e.g. two internal
// packages whose last path segment is both "trace"), we assign unique aliases
// like "trace0", "trace1" and rewrite the Symbol / usage accordingly.
// ---------------------------------------------------------------------------

type resolvedImport struct {
	alias  string // final identifier used in code (may equal default)
	path   string
	symbol string // full statement, e.g. `_ = uuid.New()`
}

// resolveImports takes third-party deps and internal packages for a single
// generated package, resolves any identifier collisions, and returns the
// final import entries and usage statements.
func resolveImports(tpDeps []ThirdPartyDep, intPkgs []Package) (tpResolved, intResolved []resolvedImport) {
	// Collect all (wantedIdent, importPath) pairs – third-party first, then internal.
	type slot struct {
		defaultIdent  string
		explicitAlias string // non-empty = caller wants this specific name
		importPath    string
		// third-party specific
		symbol string // raw expression from ThirdPartyDep
		// internal: symbol is always "<ident>.Pkg"
	}

	slots := make([]slot, 0, len(tpDeps)+len(intPkgs))
	for _, d := range tpDeps {
		slots = append(slots, slot{
			defaultIdent:  d.defaultIdent(),
			explicitAlias: d.ImportAlias,
			importPath:    d.ImportPath,
			symbol:        d.Symbol,
		})
	}
	for _, p := range intPkgs {
		slots = append(slots, slot{
			defaultIdent: p.Name,
			importPath:   p.ImportPath,
			symbol:       p.Name + ".Pkg", // will be rewritten if aliased
		})
	}

	// Determine the desired identifier for each slot (respecting explicit aliases).
	desired := make([]string, len(slots))
	for i, s := range slots {
		if s.explicitAlias != "" {
			desired[i] = s.explicitAlias
		} else {
			desired[i] = s.defaultIdent
		}
	}

	// Count how many slots want each identifier.
	count := map[string]int{}
	for _, d := range desired {
		count[d]++
	}

	// Assign final aliases. Slots whose desired ident is unique keep it as-is.
	// Slots in a collision group get an indexed suffix (trace0, trace1, …).
	indexFor := map[string]int{}
	final := make([]string, len(slots))
	for i, d := range desired {
		if count[d] == 1 {
			final[i] = d
		} else {
			idx := indexFor[d]
			indexFor[d]++
			final[i] = fmt.Sprintf("%s%d", d, idx)
		}
	}

	// Build resolved slices.
	nTP := len(tpDeps)
	for i, s := range slots {
		alias := final[i]
		orig := desired[i]

		// Rewrite symbol: replace all occurrences of "orig." with "alias."
		sym := s.symbol
		if alias != orig {
			sym = strings.ReplaceAll(sym, orig+".", alias+".")
		}

		ri := resolvedImport{
			alias:  alias,
			path:   s.importPath,
			symbol: "_ = " + sym,
		}

		if i < nTP {
			// Only emit an explicit alias in the import if it differs from
			// what Go would pick naturally (last path element).
			naturalName := tpDeps[i].defaultIdent()
			if alias == naturalName && tpDeps[i].ImportAlias == "" {
				ri.alias = "" // no alias needed in import line
			}
			tpResolved = append(tpResolved, ri)
		} else {
			// For internal deps, the natural name is the package name (last
			// path element), which is what Go picks automatically from the
			// import path. Only emit alias when it changed.
			naturalName := intPkgs[i-nTP].Name
			if alias == naturalName {
				ri.alias = ""
			}
			intResolved = append(intResolved, ri)
		}
	}

	return
}

// ---------------------------------------------------------------------------
// Code templates
// ---------------------------------------------------------------------------

type importEntry struct {
	Alias string
	Path  string
}

func (e importEntry) Line() string {
	if e.Alias != "" {
		return fmt.Sprintf("%s %q", e.Alias, e.Path)
	}
	return fmt.Sprintf("%q", e.Path)
}

type srcData struct {
	PkgName     string
	Imports     []importEntry
	HasImports  bool
	Usages      []string
	EmbedImport bool
}

type testData struct {
	PkgName        string
	TestFuncSuffix string
	Imports        []importEntry
	Usages         []string
}

type xtestData struct {
	PkgName        string
	TestFuncSuffix string
	PkgImport      string
	PkgAlias       string // explicit alias for the pkg under test (always set)
	Imports        []importEntry
	Usages         []string
}

const srcTmpl = `package {{.PkgName}}
{{if .HasImports}}
import (
{{- range .Imports}}
	{{.Line}}
{{- end}}
)
{{end}}
// Pkg is the exported identifier for package {{.PkgName}}.
var Pkg = "{{.PkgName}}"
{{if .EmbedImport}}
//go:embed assets
var pkgAssets embed.FS
{{end}}
func init() {
{{- range .Usages}}
	{{.}}
{{- end}}
{{- if .EmbedImport}}
	_, _ = pkgAssets.ReadDir(".")
{{- end}}
}
`

const testTmpl = `package {{.PkgName}}

import (
	"testing"
{{- range .Imports}}
	{{.Line}}
{{- end}}
)

func Test{{.TestFuncSuffix}}(t *testing.T) {
{{- range .Usages}}
	{{.}}
{{- end}}
	_ = Pkg
}
`

const xtestTmpl = `package {{.PkgName}}_test

import (
	"testing"
	{{.PkgAlias}} "{{.PkgImport}}"
{{- range .Imports}}
	{{.Line}}
{{- end}}
)

func Test{{.TestFuncSuffix}}X(t *testing.T) {
{{- range .Usages}}
	{{.}}
{{- end}}
	_ = {{.PkgAlias}}.Pkg
}
`

const goModTmpl = `module {{.Module}}

go 1.21

require (
{{- range .Requires}}
	{{.Mod}} {{.Ver}}
{{- end}}
)
`

var (
	tmplSrc   = template.Must(template.New("src").Parse(srcTmpl))
	tmplTest  = template.Must(template.New("test").Parse(testTmpl))
	tmplXTest = template.Must(template.New("xtest").Parse(xtestTmpl))
	tmplGoMod = template.Must(template.New("gomod").Parse(goModTmpl))
)

// ---------------------------------------------------------------------------
// File generation
// ---------------------------------------------------------------------------

func writeFile(path string, tmpl *template.Template, data interface{}) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return tmpl.Execute(f, data)
}

func capitalize(s string) string {
	if s == "" {
		return ""
	}
	return strings.ToUpper(s[:1]) + s[1:]
}

// resolvedToImportEntries converts []resolvedImport → importEntry slice + usages slice.
func resolvedToEntries(rs []resolvedImport) (entries []importEntry, usages []string) {
	for _, r := range rs {
		entries = append(entries, importEntry{Alias: r.alias, Path: r.path})
		usages = append(usages, r.symbol)
	}
	return
}

func generatePackage(outRoot string, pkg Package, allPkgs []Package) error {
	dir := filepath.Join(outRoot, pkg.RelDir)

	// Resolve internal dep packages.
	intPkgs := make([]Package, len(pkg.InternalDeps))
	for i, idx := range pkg.InternalDeps {
		intPkgs[i] = allPkgs[idx]
	}

	// Resolve third-party + internal imports with collision-free aliases.
	tpResolved, intResolved := resolveImports(pkg.ThirdPartyDeps, intPkgs)

	// Build the merged import + usage lists for the source file.
	var allImports []importEntry
	var allUsages []string
	embedImport := pkg.EmbedFiles
	if embedImport {
		allImports = append(allImports, importEntry{Path: "embed"})
	}
	tpEntries, tpUsages := resolvedToEntries(tpResolved)
	intEntries, intUsages := resolvedToEntries(intResolved)
	allImports = append(allImports, tpEntries...)
	allImports = append(allImports, intEntries...)
	allUsages = append(allUsages, tpUsages...)
	allUsages = append(allUsages, intUsages...)

	// Generate embedded asset files.
	if pkg.EmbedFiles {
		assetsDir := filepath.Join(dir, "assets")
		if err := os.MkdirAll(assetsDir, 0o755); err != nil {
			return fmt.Errorf("assets dir: %w", err)
		}
		for _, name := range []string{"config.yaml", "schema.json"} {
			var content string
			if name == "schema.json" {
				content = fmt.Sprintf(`{"package":%q,"generated":true}`, pkg.Name) + "\n"
			} else {
				content = fmt.Sprintf("# asset for package %s\n# file: %s\ngenerated: true\n", pkg.Name, name)
			}
			if err := os.WriteFile(filepath.Join(assetsDir, name), []byte(content), 0o644); err != nil {
				return fmt.Errorf("asset %s: %w", name, err)
			}
		}
	}

	// Write main source file.
	if err := writeFile(filepath.Join(dir, pkg.Name+".go"), tmplSrc, srcData{
		PkgName:     pkg.Name,
		Imports:     allImports,
		HasImports:  len(allImports) > 0,
		Usages:      allUsages,
		EmbedImport: embedImport,
	}); err != nil {
		return err
	}

	funcSuffix := capitalize(pkg.Name)

	// For xtest files: the package under test always needs an explicit alias
	// to ensure its identifier is unambiguous (it might share a name with a
	// third-party or internal dep used in the test).
	xtestPkgAlias := pkg.Name + "_pkg"

	writeTest := func(fileBase string) error {
		testTP, testInt := resolveImports(pkg.ThirdPartyTestDeps, nil)
		testEntries, testUsages := resolvedToEntries(testTP)
		_ = testInt
		return writeFile(filepath.Join(dir, fileBase+"_test.go"), tmplTest, testData{
			PkgName:        pkg.Name,
			TestFuncSuffix: funcSuffix,
			Imports:        testEntries,
			Usages:         testUsages,
		})
	}
	writeXTest := func(fileBase string) error {
		testTP, testInt := resolveImports(pkg.ThirdPartyTestDeps, nil)
		testEntries, testUsages := resolvedToEntries(testTP)
		_ = testInt
		return writeFile(filepath.Join(dir, fileBase+"_xtest_test.go"), tmplXTest, xtestData{
			PkgName:        pkg.Name,
			TestFuncSuffix: funcSuffix,
			PkgImport:      pkg.ImportPath,
			PkgAlias:       xtestPkgAlias,
			Imports:        testEntries,
			Usages:         testUsages,
		})
	}

	switch pkg.TestKind {
	case TestOnly:
		if err := writeTest(pkg.Name); err != nil {
			return err
		}
	case XTestOnly:
		if err := writeXTest(pkg.Name); err != nil {
			return err
		}
	case TestBoth:
		if err := writeTest(pkg.Name); err != nil {
			return err
		}
		if err := writeXTest(pkg.Name); err != nil {
			return err
		}
	}

	return nil
}

// ---------------------------------------------------------------------------
// go.mod generation
// ---------------------------------------------------------------------------

type requireLine struct {
	Mod string
	Ver string
}

func generateGoMod(outRoot, module string, pkgs []Package) error {
	needed := map[string]bool{}
	collect := func(deps []ThirdPartyDep) {
		for _, dep := range deps {
			needed[moduleForImport(dep.ImportPath)] = true
		}
	}
	for _, pkg := range pkgs {
		collect(pkg.ThirdPartyDeps)
		collect(pkg.ThirdPartyTestDeps)
	}

	var reqs []requireLine
	for mod := range needed {
		reqs = append(reqs, requireLine{Mod: mod, Ver: thirdPartyModules[mod]})
	}
	sort.Slice(reqs, func(i, j int) bool { return reqs[i].Mod < reqs[j].Mod })

	f, err := os.Create(filepath.Join(outRoot, "go.mod"))
	if err != nil {
		return err
	}
	defer f.Close()
	return tmplGoMod.Execute(f, struct {
		Module   string
		Requires []requireLine
	}{module, reqs})
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	var cfg Config
	flag.Int64Var(&cfg.Seed, "seed", 42, "Random seed for deterministic output")
	flag.StringVar(&cfg.Out, "out", "./genrepo", "Output directory")
	flag.StringVar(&cfg.Module, "module", "example.com/genrepo", "Go module name")
	flag.IntVar(&cfg.NumPkgs, "pkgs", 20, "Number of packages to generate")
	flag.IntVar(&cfg.MaxDepth, "max-depth", 4, "Maximum directory nesting depth")
	flag.Parse()

	r := rand.New(rand.NewSource(cfg.Seed))

	usedPaths := map[string]bool{}
	pkgs := make([]Package, cfg.NumPkgs)
	for i := range pkgs {
		relDir := buildRelDir(r, cfg.MaxDepth, usedPaths)
		parts := strings.Split(relDir, "/")
		name := parts[len(parts)-1]
		pkgs[i] = Package{
			Name:               name,
			RelDir:             relDir,
			ImportPath:         cfg.Module + "/" + relDir,
			InternalDeps:       pickInternalDeps(r, i),
			ThirdPartyDeps:     pickThirdPartyDeps(r),
			TestKind:           pickTestKind(r),
			ThirdPartyTestDeps: pickThirdPartyDeps(r),
			EmbedFiles:         pickEmbedFiles(r),
		}
	}

	if err := os.MkdirAll(cfg.Out, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "mkdir: %v\n", err)
		os.Exit(1)
	}

	if err := generateGoMod(cfg.Out, cfg.Module, pkgs); err != nil {
		fmt.Fprintf(os.Stderr, "go.mod: %v\n", err)
		os.Exit(1)
	}

	embedCount := 0
	for _, pkg := range pkgs {
		if pkg.EmbedFiles {
			embedCount++
		}
		fmt.Printf("  pkg %-44s  deps=%-10v  tests=%v  embed=%v\n",
			pkg.ImportPath, pkg.InternalDeps, pkg.TestKind, pkg.EmbedFiles)
		if err := generatePackage(cfg.Out, pkg, pkgs); err != nil {
			fmt.Fprintf(os.Stderr, "package %s: %v\n", pkg.ImportPath, err)
			os.Exit(1)
		}
	}

	fmt.Printf("\nGenerated %d packages (%d with embedded assets) in %s\n",
		len(pkgs), embedCount, cfg.Out)
	fmt.Printf("Module: %s\n", cfg.Module)
	fmt.Println("\nRun `go mod tidy` inside the generated directory to fetch dependencies.")
}
