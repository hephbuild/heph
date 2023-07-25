package main

import (
	. "e2e/lib"
)

// This is a sanity test that all spans reach hephcloud
func main() {
	Scenario(func() error {
		return RunO("//:deps", RunOpts{Silent: true})
	}, true, 3, -1)
}
