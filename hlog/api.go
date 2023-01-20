package hlog

import "heph/hlog/log"

type Level = log.Level

var ParseLevel = log.ParseLevel

const (
	InvalidLevel = log.InvalidLevel
	TraceLevel   = log.TraceLevel
	DebugLevel   = log.DebugLevel
	InfoLevel    = log.InfoLevel
	WarnLevel    = log.WarnLevel
	ErrorLevel   = log.ErrorLevel
	PanicLevel   = log.PanicLevel
	FatalLevel   = log.FatalLevel
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
