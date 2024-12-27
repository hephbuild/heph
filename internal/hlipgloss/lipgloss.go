package hlipgloss

import (
	"github.com/muesli/termenv"
	"os"
	"strconv"
)

var forcetty bool

func init() {
	forcetty, _ = strconv.ParseBool(os.Getenv("FORCE_TTY"))
}

func EnvForceTTY() termenv.OutputOption {
	if forcetty {
		return termenv.WithTTY(true)
	}

	return func(output *termenv.Output) {}
}
