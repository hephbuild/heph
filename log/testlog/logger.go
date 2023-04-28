package testlog

import (
	"github.com/hephbuild/heph/log/liblog"
	"os"
	"testing"
)

func NewCollector(t testing.TB) liblog.Collector {
	return console{t: t, fmt: liblog.NewConsoleFormatter(os.Stderr)}
}

func NewLogger(t testing.TB) liblog.Logger {
	return liblog.NewLogger(liblog.NewLock(liblog.NewCore(NewCollector(t))))
}

type console struct {
	t   testing.TB
	fmt *liblog.ConsoleFormatter
}

func (c console) Write(entry liblog.Entry) error {
	buf := c.fmt.Format(entry)
	defer buf.Free()

	c.t.Logf("%s", buf.Bytes())
	return nil
}
