package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"go/build"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

func listImports() {
	root := Env.Root

	err := os.Chdir(root)
	if err != nil {
		panic(err)
	}

	b, _ := json.Marshal(FilesOrigin)
	fmt.Println("files origin:", hashBytes(b))
	fmt.Println("src:", hashString(os.Getenv("SRC")))
	modRoot := findGoModRoot(root)
	fmt.Println("gomod:", hashFile(filepath.Join(modRoot, "go.mod")))
	fmt.Println("gosum:", hashFile(filepath.Join(modRoot, "go.sum")))

	build.Default.UseAllFiles = true

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
		fmt.Fprintf(os.Stderr, "found %v xtest imports\n", len(p.XTestImports))

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

			f, err := os.Open(filepath.Join(path, file))
			if err != nil {
				return err
			}

			scanner := bufio.NewScanner(f)
			for scanner.Scan() {
				t := scanner.Text()
				if strings.HasPrefix(t, "//go:") || strings.Contains(t, "+build") {
					fmt.Println("BUILDCONSTRAINT", t)
				}
			}

			_ = f.Close()

			if err := scanner.Err(); err != nil {
				return fmt.Errorf("%v: %w", filepath.Join(root, file), err)
			}
		}

		fmt.Println("+++ Imports")
		sort.Strings(p.Imports)
		for _, i := range p.Imports {
			fmt.Println(i)
		}

		fmt.Println("+++ TestImports")
		sort.Strings(p.TestImports)
		for _, i := range p.TestImports {
			fmt.Println(i)
		}

		fmt.Println("+++ XTestImports")
		sort.Strings(p.XTestImports)
		for _, i := range p.XTestImports {
			fmt.Println(i)
		}

		return nil
	})

	if err != nil {
		panic(err)
	}
}
