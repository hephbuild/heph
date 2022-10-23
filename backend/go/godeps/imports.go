package main

import (
	"encoding/json"
	"fmt"
	"go/build"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
)

func listImports() {
	root := os.Getenv("ROOT")

	err := os.Chdir(root)
	if err != nil {
		panic(err)
	}

	json.NewEncoder(os.Stdout).Encode(FilesOrigin)
	fmt.Println()
	fmt.Println(os.Getenv("SRC"))

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
