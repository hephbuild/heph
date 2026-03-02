package hlipgloss

import (
	"io"
	"os"
	"strconv"

	"github.com/charmbracelet/colorprofile"
)

var forcetty bool

func init() {
	forcetty, _ = strconv.ParseBool(os.Getenv("FORCE_TTY"))
}

func NewWriter(w io.Writer) *colorprofile.Writer {
	p := colorprofile.NewWriter(w, os.Environ())
	if forcetty {
		p.Profile = colorprofile.TrueColor
	}

	return p
}
