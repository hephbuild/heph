package main

import (
	"os"
	"path/filepath"
)

func genBuild() {
	ParseConfig(os.Args[2])
	InitStd()

	unitsPerDir := map[string][]RenderUnit{}

	for _, unit := range generate() {
		unitsPerDir[unit.Dir] = append(unitsPerDir[unit.Dir], unit)
	}

	for dir, units := range unitsPerDir {
		err := os.MkdirAll(filepath.Join(Env.Sandbox, dir), os.ModePerm)
		if err != nil {
			panic(err)
		}

		f, err := os.Create(filepath.Join(Env.Sandbox, dir, "BUILD"))
		if err != nil {
			panic(err)
		}

		for _, unit := range units {
			unit.Render(f)
		}

		f.Close()
	}
}
