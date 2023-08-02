package main

import (
	. "e2e/lib"
	"fmt"
)

// This is a sanity test that group with transitives works
func main() {
	Must(CleanSetup())

	fmt.Println("### test_unamed")
	Must(Run("//:test_unamed"))

	fmt.Println("### test_named")
	Must(Run("//:test_named"))
}
