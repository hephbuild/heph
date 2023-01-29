package utils

import (
	"os"
	"os/exec"
	"strconv"
)

const DevVersion = "dev"

var Version = version

func IsDevVersion() bool {
	return Version == DevVersion
}

// For dev reasons we may want to force heph inside the sandbox to use the wrapper script so it recompiles itself
var HephFromPath bool

func init() {
	if s := os.Getenv("HEPH_FROM_PATH"); s != "" {
		HephFromPath, _ = strconv.ParseBool(s)
	}
}

func Executable() string {
	if HephFromPath && IsDevVersion() {
		ex, err := exec.LookPath("heph")
		if err != nil {
			panic(err)
		}
		return ex
	}

	ex, err := os.Executable()
	if err != nil {
		panic(err)
	}

	return ex
}
