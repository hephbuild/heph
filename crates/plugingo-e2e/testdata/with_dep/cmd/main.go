package main

import (
	"fmt"

	"example.com/with_dep/lib"
)

func main() {
	fmt.Println(lib.Greet("world"))
}
