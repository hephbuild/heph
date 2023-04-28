package liblog

import "os"

var defaultLoggerFunc func() Logger

func SetDefaultLogger(f func() Logger) {
	if defaultLoggerFunc != nil {
		panic("default logger already registered")
	}

	defaultLoggerFunc = f
}

// Logger provider by default so that it doesnt bomb in tests for example
var defaultLogger = NewLogger(NewLock(NewCore(NewConsole(os.Stderr))))

func Default() Logger {
	if defaultLoggerFunc == nil {
		return defaultLogger
	}

	return defaultLoggerFunc()
}
