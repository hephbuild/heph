package main

import (
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

	importsm := map[string]struct{}{}

	err = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if !d.IsDir() {
			return nil
		}

		fmt.Fprintf(os.Stderr, "DIR: %v\n", path)

		p, err := build.Default.Import(".", path, 0)
		if err != nil {
			fmt.Fprintf(os.Stderr, "import: %v\n", err)
			return nil
		}

		fmt.Fprintf(os.Stderr, "found %v imports\n", len(p.Imports))
		fmt.Fprintf(os.Stderr, "found %v test imports\n", len(p.TestImports))

		for _, i := range p.Imports {
			importsm[i] = struct{}{}
		}

		for _, i := range p.TestImports {
			importsm[i] = struct{}{}
		}

		return nil
	})

	if err != nil {
		panic(err)
	}

	imports := make([]string, 0)

	sort.Strings(imports)

	for _, i := range imports {
		fmt.Println(i)
	}
}
