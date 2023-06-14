package log

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/liblog"
	"os"
)

var level = liblog.InfoLevel
var defaultLogger = newDefaultLogger()

func newDefaultLogger() liblog.Logger {
	return liblog.NewLogger(liblog.NewLevelEnabler(liblog.NewLock(liblog.NewCore(liblog.NewConsole(os.Stderr))), IsLevelEnabled))
}

var renderer *lipgloss.Renderer
var divert *divertCore

func SetDiversion(ch chan liblog.Entry) {
	divert.divert(ch)
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

	renderer = liblog.NewConsoleRenderer(Writer())

	divert = newDivertCore(liblog.NewLock(liblog.NewCore(liblog.NewConsoleWith(Writer(), renderer))))

	defaultLogger = liblog.NewLogger(liblog.NewLevelEnabler(divert, IsLevelEnabled))
}

func Renderer() *lipgloss.Renderer {
	return renderer
}

func Writer() *os.File {
	return os.Stderr
}

func Default() liblog.Logger {
	return liblog.Default()
}

func Cleanup() {
	//defaultLogger.Sync()
}
