package main

import (
	"os"
	"path/filepath"
)

func genBuild() {
	ParseConfig(os.Args[2])
	InitStd()

	unitsPerPackage := map[string][]RenderUnit{}

	for _, unit := range generate() {
		unitsPerPackage[unit.Package] = append(unitsPerPackage[unit.Package], unit)
	}

	for pkg, units := range unitsPerPackage {
		err := os.MkdirAll(filepath.Join(Env.Sandbox, pkg), os.ModePerm)
		if err != nil {
			panic(err)
		}

		f, err := os.Create(filepath.Join(Env.Sandbox, pkg, "BUILD"))
		if err != nil {
			panic(err)
		}

		for _, unit := range units {
			unit.Render(f)
		}

		f.Close()
	}
}
