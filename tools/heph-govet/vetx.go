package main

// Synthesized vetx files for imports heph has no facts for.
//
// unitchecker (x/tools ≥ v0.50) reads each direct import's *types* from its
// PackageVetx file, the same file that carries its analysis facts; the cfg's
// PackageFile (compiled archive) map is no longer read. heph produces a vetx file
// only for the packages it lints itself, so the standard library, and any
// dependency in a module that does not opt into linting, has an archive but no
// vetx file. unitchecker then fails outright:
//
//	could not import net/http (no package vetx file for "net/http")
//
// Before unitchecker runs, heph-govet writes a vetx file for each such direct
// import: its types are read from the archive, and its facts are empty, which is
// the "no facts for this dependency" that unitchecker treated a missing entry as
// before. The rewritten cfg is written beside the original, which is left as is.

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"golang.org/x/tools/go/analysis/unitchecker"
	"golang.org/x/tools/go/gcexportdata"
)

// vetxMagic and the layout below mirror unitchecker's (unexported) vetx format:
// "vetx", little-endian uint32 lengths of the types and facts sections, then the
// sections themselves. Types are indexed export data (gcimporter.IExportData).
const vetxMagic = "vetx"

// synthDir is the directory, beside the cfg, that holds the synthesized files.
const synthDir = "heph-govet-vetx"

// withVetxForEveryImport returns the path of a cfg in which every direct import
// of the package has a PackageVetx entry. It returns cfgPath unchanged when none
// is missing.
func withVetxForEveryImport(cfgPath string) (string, error) {
	data, err := os.ReadFile(cfgPath)
	if err != nil {
		return "", fmt.Errorf("read unitchecker cfg: %w", err)
	}
	var cfg unitchecker.Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return "", fmt.Errorf("decode unitchecker cfg %s: %w", cfgPath, err)
	}

	missing, err := importsWithoutVetx(&cfg)
	if err != nil {
		return "", err
	}
	if len(missing) == 0 {
		return cfgPath, nil
	}

	out := filepath.Join(filepath.Dir(cfgPath), synthDir)
	if err := os.MkdirAll(out, 0o755); err != nil {
		return "", fmt.Errorf("create %s: %w", out, err)
	}
	fset := token.NewFileSet()
	imp := importer.ForCompiler(fset, "gc", func(path string) (io.ReadCloser, error) {
		file, ok := cfg.PackageFile[path]
		if !ok {
			return nil, fmt.Errorf("no package file for %q", path)
		}
		return os.Open(file)
	})
	if cfg.PackageVetx == nil {
		cfg.PackageVetx = map[string]string{}
	}
	for i, pkgPath := range missing {
		pkg, err := imp.Import(pkgPath)
		if err != nil {
			return "", fmt.Errorf("load types of %q from its archive: %w", pkgPath, err)
		}
		file := filepath.Join(out, strconv.Itoa(i)+".vetx")
		if err := writeTypesOnlyVetx(fset, pkg, file); err != nil {
			return "", fmt.Errorf("write vetx file for %q: %w", pkgPath, err)
		}
		cfg.PackageVetx[pkgPath] = file
	}

	data, err = json.Marshal(&cfg)
	if err != nil {
		return "", fmt.Errorf("encode unitchecker cfg: %w", err)
	}
	// unitchecker requires the cfg argument to end in ".cfg".
	rewritten := filepath.Join(out, "unitchecker.cfg")
	if err := os.WriteFile(rewritten, data, 0o644); err != nil {
		return "", fmt.Errorf("write unitchecker cfg: %w", err)
	}
	return rewritten, nil
}

// importsWithoutVetx returns the package paths of the direct imports that have a
// compiled archive but no vetx file, in first-seen order. Direct imports are the
// only ones unitchecker reads a vetx file for (types of indirect ones travel in
// the direct imports' export data), so the transitive set is never loaded.
func importsWithoutVetx(cfg *unitchecker.Config) ([]string, error) {
	fset := token.NewFileSet()
	seen := map[string]bool{}
	var missing []string
	for _, name := range cfg.GoFiles {
		f, err := parser.ParseFile(fset, name, nil, parser.ImportsOnly)
		if err != nil {
			return nil, fmt.Errorf("read imports of %s: %w", name, err)
		}
		for _, spec := range f.Imports {
			ip, err := strconv.Unquote(spec.Path.Value)
			if err != nil {
				return nil, fmt.Errorf("import path %s in %s: %w", spec.Path.Value, name, err)
			}
			pkgPath, ok := cfg.ImportMap[ip]
			if !ok {
				pkgPath = ip // unitchecker reports the unresolved import itself
			}
			if pkgPath == "unsafe" || seen[pkgPath] {
				continue
			}
			seen[pkgPath] = true
			if _, ok := cfg.PackageVetx[pkgPath]; ok {
				continue
			}
			if _, ok := cfg.PackageFile[pkgPath]; ok {
				missing = append(missing, pkgPath)
			}
		}
	}
	return missing, nil
}

// writeTypesOnlyVetx writes pkg's types as a vetx file with an empty facts
// section, which unitchecker decodes as "no facts".
func writeTypesOnlyVetx(fset *token.FileSet, pkg *types.Package, filename string) error {
	var exp bytes.Buffer
	if err := gcexportdata.Write(&exp, fset, pkg); err != nil {
		return fmt.Errorf("export types: %w", err)
	}
	// gcexportdata.Write is "i" + IExportData; a vetx file holds the latter.
	typesData, ok := strings.CutPrefix(exp.String(), "i")
	if !ok {
		return fmt.Errorf("unexpected export data header %q", exp.Bytes()[:min(exp.Len(), 1)])
	}
	var buf bytes.Buffer
	buf.Grow(12 + len(typesData))
	buf.WriteString(vetxMagic)
	var lens [8]byte
	binary.LittleEndian.PutUint32(lens[0:4], uint32(len(typesData)))
	binary.LittleEndian.PutUint32(lens[4:8], 0) // no facts
	buf.Write(lens[:])
	buf.WriteString(typesData)
	return os.WriteFile(filename, buf.Bytes(), 0o644)
}
