package main

import (
	. "e2e/lib"
	"strings"
)

// This is a sanity test that restore cache on a file used as input & output works
func main() {
	Must(CleanSetup())

	Must(WriteFile("file.txt", "initial"))

	outputs := MustV(RunOutput("//:hello"))
	expected := MustV(FileContent(outputs[0]))
	eleft, eright, _ := strings.Cut(expected, ":")

	Must(AssertEqual("initial ran", eleft))

	Must(WriteFile("file.txt", "force"))

	outputs = MustV(RunOutput("//:hello"))
	actual := MustV(FileContent(outputs[0]))
	aleft, aright, _ := strings.Cut(actual, ":")

	Must(AssertEqual("force ran", aleft))

	Must(AssertEqual(aright, eright))
}
