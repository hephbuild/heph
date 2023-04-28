package log

import (
	"github.com/hephbuild/heph/log/liblog"
	"os"
)

var level = liblog.InfoLevel
var defaultLogger = newDefaultLogger()

func newDefaultLogger() liblog.Logger {
	return liblog.NewLogger(liblog.NewLevelEnabler(liblog.NewLock(liblog.NewCore(liblog.NewConsole(os.Stderr))), IsLevelEnabled))
}

var tuiInterceptCore *interceptCore

func SetPrint(width int, f func(string)) {
	tuiInterceptCore.width = width
	tuiInterceptCore.print = f
}

func SetLevel(lvl liblog.Level) {
	level = lvl
}

func IsLevelEnabled(lvl liblog.Level) bool {
	return lvl >= level
}

func Setup() {
	liblog.SetDefaultLogger(func() liblog.Logger {
		return defaultLogger
	})

	tuiInterceptCore = newInterceptCore(os.Stderr, liblog.NewLock(liblog.NewCore(liblog.NewConsole(os.Stderr))))

	defaultLogger = liblog.NewLogger(liblog.NewLevelEnabler(tuiInterceptCore, IsLevelEnabled))
}

func Default() liblog.Logger {
	return liblog.Default()
}

func Cleanup() {
	//defaultLogger.Sync()
}
