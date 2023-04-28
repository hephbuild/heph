package log

import (
	"github.com/hephbuild/heph/log/liblog"
	"github.com/muesli/reflow/wrap"
	"io"
)

func newInterceptCore(w io.Writer, core liblog.Core) *interceptCore {
	return &interceptCore{
		fmt:  liblog.NewConsoleFormatter(w),
		core: core,
	}
}

type interceptCore struct {
	fmt   *liblog.ConsoleFormatter
	core  liblog.Core
	width int
	print func(string)
}

func (t interceptCore) Enabled(l liblog.Level) bool {
	return t.core.Enabled(l)
}

func (t interceptCore) Log(entry liblog.Entry) error {
	printf := t.print
	if printf != nil {
		termWidth := t.width

		buf := t.fmt.Format(entry)
		defer buf.Free()

		b := buf.Bytes()

		if termWidth > 0 && len(b) > termWidth {
			b = wrap.Bytes(b, termWidth)
		}

		go printf(string(b))
		return nil
	}

	return t.core.Log(entry)
}

var _ liblog.Core = (*interceptCore)(nil)
