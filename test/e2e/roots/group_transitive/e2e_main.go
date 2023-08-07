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

	fmt.Println("### test_unamed-group_named-dep")
	output := MustV(RunOutput("//:test_unamed-group_named-dep"))
	Must(AssertFileContentEqual(output[0], "SRC=t2"))

	fmt.Println("### test_named-group_named-dep")
	output = MustV(RunOutput("//:test_named-group_named-dep"))
	Must(AssertFileContentEqual(output[0], "SRC=t2"))

	fmt.Println("### test_named-group_unnamed-dep")
	output = MustV(RunOutput("//:test_named-group_unnamed-dep"))
	Must(AssertFileContentEqual(output[0], "SRC_T1=t1\nSRC_T2=t2"))

	fmt.Println("### test_deep-group")
	output = MustV(RunOutput("//:test_deep-group"))
	Must(AssertFileContentEqual(output[0], "SRC=t1 t2 t3"))
}
