package main

import (
	. "e2e/lib"
	"fmt"
	"os"
)

// This is a sanity test that running a target works
func main() {
	Must(CleanSetup())

	{ // pass_env
		fmt.Println("# pass_env")
		Must(os.Setenv("SOME_VAR", "v1"))

		outputs := MustV(RunOutput("//:pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput1 := MustV(TargetCacheInputHash("//:pass"))

		Must(os.Setenv("SOME_VAR", "v2"))

		outputs = MustV(RunOutput("//:pass"))
		Must(AssertFileContentEqual(outputs[0], "v2"))
		hashInput2 := MustV(TargetCacheInputHash("//:pass"))

		Must(AssertNotEqual(hashInput2, hashInput1))
	}

	{ // runtime_pass_env
		fmt.Println("# runtime_pass_env")
		Must(os.Setenv("SOME_VAR", "v1"))

		outputs := MustV(RunOutput("//:runtime_pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput1 := MustV(TargetCacheInputHash("//:runtime_pass"))

		Must(os.Setenv("SOME_VAR", "v2"))

		outputs = MustV(RunOutput("//:runtime_pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput2 := MustV(TargetCacheInputHash("//:runtime_pass"))

		Must(AssertEqual(hashInput2, hashInput1))
	}

	{ // transitive pass_env
		fmt.Println("# transitive pass_env")
		Must(os.Setenv("SOME_VAR", "v1"))

		outputs := MustV(RunOutput("//:tr_pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput1 := MustV(TargetCacheInputHash("//:tr_pass"))

		Must(os.Setenv("SOME_VAR", "v2"))

		outputs = MustV(RunOutput("//:tr_pass"))
		Must(AssertFileContentEqual(outputs[0], "v2"))
		hashInput2 := MustV(TargetCacheInputHash("//:tr_pass"))

		Must(AssertNotEqual(hashInput2, hashInput1))
	}

	{ // transitive runtime_pass_env
		fmt.Println("# transitive runtime_pass_env")
		Must(os.Setenv("SOME_VAR", "v1"))

		outputs := MustV(RunOutput("//:tr_runtime_pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput1 := MustV(TargetCacheInputHash("//:tr_runtime_pass"))

		Must(os.Setenv("SOME_VAR", "v2"))

		outputs = MustV(RunOutput("//:tr_runtime_pass"))
		Must(AssertFileContentEqual(outputs[0], "v1"))
		hashInput2 := MustV(TargetCacheInputHash("//:tr_runtime_pass"))

		Must(AssertEqual(hashInput2, hashInput1))
	}
}
