package hlog

import (
	"github.com/muesli/reflow/wrap"
	"heph/hlog/log"
	"io"
)

func newInterceptCore(w io.Writer, core log.Core) *interceptCore {
	return &interceptCore{
		fmt:  log.NewConsoleFormatter(w),
		core: core,
	}
}

type interceptCore struct {
	fmt   *log.Formatter
	core  log.Core
	width int
	print func(string)
}

func (t interceptCore) Enabled(l log.Level) bool {
	return t.core.Enabled(l)
}

func (t interceptCore) Log(entry log.Entry) error {
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

var _ log.Core = (*interceptCore)(nil)
