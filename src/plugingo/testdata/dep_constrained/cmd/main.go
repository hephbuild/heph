package cmd

import (
	"fmt"

	"example.com/dep_constrained/lib"
)

func Run() {
	fmt.Println(lib.Greet("world"))
}
