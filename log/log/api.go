package log

import "github.com/hephbuild/heph/log/liblog"

type Level = liblog.Level

var ParseLevel = liblog.ParseLevel

const (
	InvalidLevel = liblog.InvalidLevel
	TraceLevel   = liblog.TraceLevel
	DebugLevel   = liblog.DebugLevel
	InfoLevel    = liblog.InfoLevel
	WarnLevel    = liblog.WarnLevel
	ErrorLevel   = liblog.ErrorLevel
	PanicLevel   = liblog.PanicLevel
	FatalLevel   = liblog.FatalLevel
)

func Trace(args ...any) {
	defaultLogger.Trace(args...)
}

func Tracef(f string, args ...interface{}) {
	defaultLogger.Tracef(f, args...)
}

func Debug(args ...any) {
	defaultLogger.Debug(args...)
}

func Debugf(f string, args ...interface{}) {
	defaultLogger.Debugf(f, args...)
}

func Info(args ...any) {
	defaultLogger.Info(args...)
}

func Infof(f string, args ...interface{}) {
	defaultLogger.Infof(f, args...)
}

func Warn(args ...any) {
	defaultLogger.Warn(args...)
}

func Warnf(f string, args ...interface{}) {
	defaultLogger.Warnf(f, args...)
}

func Error(args ...any) {
	defaultLogger.Error(args...)
}

func Errorf(f string, args ...interface{}) {
	defaultLogger.Errorf(f, args...)
}

func Fatal(args ...any) {
	defaultLogger.Fatal(args...)
}

func Fatalf(f string, args ...interface{}) {
	defaultLogger.Fatalf(f, args...)
}
