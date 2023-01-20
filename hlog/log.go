package hlog

import (
	"heph/hlog/log"
	"os"
)

var level = log.InfoLevel
var defaultLogger = newDefaultLogger()

func newDefaultLogger() *log.Logger {
	return log.NewLogger(log.NewLevelEnabler(log.NewLock(log.NewCore(log.NewConsole(os.Stderr))), IsLevelEnabled))
}

var tuiInterceptCore *interceptCore

func SetPrint(width int, f func(string)) {
	tuiInterceptCore.width = width
	tuiInterceptCore.print = f
}

func SetLevel(lvl log.Level) {
	level = lvl
}

func IsLevelEnabled(lvl log.Level) bool {
	return lvl >= level
}

func Setup() {
	tuiInterceptCore = newInterceptCore(os.Stderr, log.NewLock(log.NewCore(log.NewConsole(os.Stderr))))

	defaultLogger = log.NewLogger(log.NewLevelEnabler(tuiInterceptCore, IsLevelEnabled))
}

func Cleanup() {
	//defaultLogger.Sync()
}
