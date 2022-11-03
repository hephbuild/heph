package main

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

func renameMeta() {
	err := filepath.WalkDir(Env.Root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if d.IsDir() {
			return nil
		}

		if strings.HasSuffix(path, ".go_meta") {
			newpath := path + "_prev"

			fmt.Printf("Renaming %v to %v\n", path, newpath)

			err := os.Rename(path, newpath)
			if err != nil {
				return err
			}
		}

		return nil
	})
	if err != nil {
		panic(err)
	}
}
