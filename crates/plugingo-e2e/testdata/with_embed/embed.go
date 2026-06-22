package embedlib

import _ "embed"

//go:embed data.txt
var Data string

// Use Data so the variable isn't dead-code eliminated by vet-style checks.
func Get() string { return Data }
