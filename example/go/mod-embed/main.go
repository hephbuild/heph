package main

import (
	"embed"
	"fmt"
)

//go:embed file.txt
var file string

//go:embed *.txt
var files embed.FS

func main() {
	fmt.Println("hello")
}
