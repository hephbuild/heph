package main

import (
	"os"
)

func main() {
	switch os.Args[1] {
	case "mod":
		genBuild()
	case "imports":
		listImports()
	case "embed":
		genEmbed()
	default:
		panic("unhandled mode " + os.Args[1])
	}
}
