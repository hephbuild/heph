package main

import (
	. "e2e/lib"
)

// This is a sanity test that all spans reach hephcloud
func main() {
	Scenario(func() error {
		return RunO("-", RunOpts{Silent: true, Targets: []string{
			"//:fail0",
			"//:fail1",
			"//:fail2",
		}})
	}, true, 3, -1)
}
