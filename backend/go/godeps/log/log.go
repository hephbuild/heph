package log

import (
	"fmt"
	"os"
	"strconv"
	"strings"
)

var enabled bool

func init() {
	enabled, _ = strconv.ParseBool(os.Getenv("DEBUG"))
}

func Enabled() bool {
	return enabled
}

func Debug(args ...interface{}) {
	if !enabled {
		return
	}

	fmt.Println(args...)
}

func Debugln(args ...interface{}) {
	Debug(args...)
}

func Debugf(f string, args ...interface{}) {
	if !enabled {
		return
	}

	if !strings.HasSuffix(f, "\n") {
		f += "\n"
	}

	fmt.Printf(f, args...)
}
