package main

import (
	. "e2e/lib"
)

// This is a sanity test that all spans reach hephcloud
func main() {
	Scenario(func() error {
		return RunO("//:stresstest", RunOpts{NoPty: true})
	}, false, 1000, 6897)
}
