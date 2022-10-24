package main

import (
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"go/build"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
)

func hashString(s string) string {
	return hashBytes([]byte(s))
}

func hashBytes(b []byte) string {
	h := sha1.New()
	h.Write(b)
	return hex.EncodeToString(h.Sum(nil))
}

func hashFile(path string) string {
	b, _ := os.ReadFile(path)
	if len(b) == 0 {
		return ""
	}

	return hashBytes(b)
}

func findGoModRoot() string {
	dir, _ := os.Getwd()

	for {
		_, err := os.Stat(filepath.Join(dir, "go.mod"))
		if err == nil {
			return dir
		}

		dir = filepath.Dir(dir)
		if dir == "" {
			panic("go.mod not found")
		}
	}

	return ""
}

func listImports() {
	root := os.Getenv("ROOT")

	err := os.Chdir(root)
	if err != nil {
		panic(err)
	}

	b, _ := json.Marshal(FilesOrigin)
	fmt.Println("files origin:", hashBytes(b))
	fmt.Println("src:", hashString(os.Getenv("SRC")))
	modRoot := findGoModRoot()
	fmt.Println("gomod:", hashFile(filepath.Join(modRoot, "go.mod")))
	fmt.Println("gosum:", hashFile(filepath.Join(modRoot, "go.sum")))

	err = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if !d.IsDir() {
			return nil
		}

		fmt.Fprintf(os.Stderr, "DIR: %v\n", path)

		p, err := build.Default.ImportDir(path, 0)
		if err != nil {
			fmt.Fprintf(os.Stderr, "import: %v\n", err)
			return nil
		}

		fmt.Fprintf(os.Stderr, "found %v imports\n", len(p.Imports))
		fmt.Fprintf(os.Stderr, "found %v test imports\n", len(p.TestImports))

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}

		fmt.Println("===")
		fmt.Println("PKG", rel)

		allFiles := p.GoFiles
		allFiles = append(allFiles, p.TestGoFiles...)
		allFiles = append(allFiles, p.XTestGoFiles...)

		for _, file := range allFiles {
			fmt.Println(file)
		}

		fmt.Println("+++")

		importsm := map[string]struct{}{}

		for _, i := range p.Imports {
			importsm[i] = struct{}{}
		}

		for _, i := range p.TestImports {
			importsm[i] = struct{}{}
		}

		imports := make([]string, 0)
		for i := range importsm {
			imports = append(imports, i)
		}

		sort.Strings(imports)

		for _, i := range imports {
			fmt.Println(i)
		}

		return nil
	})

	if err != nil {
		panic(err)
	}
}
