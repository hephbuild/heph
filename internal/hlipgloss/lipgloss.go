package hlipgloss

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/muesli/termenv"
	"io"
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

func NewRenderer(w io.Writer) *lipgloss.Renderer {
	return lipgloss.NewRenderer(w, EnvForceTTY(), termenv.WithColorCache(true))
}
