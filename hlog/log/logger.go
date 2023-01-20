package log

import (
	"fmt"
	"os"
	"strings"
	"time"
)

func NewLogger(core Core) *Logger {
	return &Logger{core: core}
}

type Logger struct {
	core Core
}

func (l *Logger) logf(lvl Level, f string, args ...interface{}) {
	if !l.core.Enabled(lvl) {
		return
	}

	l.log(lvl, fmt.Sprintf(f, args...))
}

func (l *Logger) log(lvl Level, args ...any) {
	if !l.core.Enabled(lvl) {
		return
	}

	argsstr := make([]string, len(args))
	for i, arg := range args {
		switch thing := arg.(type) {
		case string:
			argsstr[i] = thing
		default:
			argsstr[i] = fmt.Sprint(thing)
		}
	}

	err := l.core.Log(Entry{
		Timestamp: time.Now(),
		Level:     lvl,
		Message:   strings.Join(argsstr, " "),
	})
	if err != nil {
		_, _ = fmt.Fprintf(os.Stderr, "error logging: %v", err)
	}
}

func (l *Logger) Trace(args ...any) {
	l.log(TraceLevel, args...)
}

func (l *Logger) Tracef(f string, args ...interface{}) {
	l.logf(TraceLevel, f, args...)
}

func (l *Logger) Debug(args ...any) {
	l.log(DebugLevel, args...)
}

func (l *Logger) Debugf(f string, args ...interface{}) {
	l.logf(DebugLevel, f, args...)
}

func (l *Logger) Info(args ...any) {
	l.log(InfoLevel, args...)
}

func (l *Logger) Infof(f string, args ...interface{}) {
	l.logf(InfoLevel, f, args...)
}

func (l *Logger) Warn(args ...any) {
	l.log(WarnLevel, args...)
}

func (l *Logger) Warnf(f string, args ...interface{}) {
	l.logf(WarnLevel, f, args...)
}

func (l *Logger) Error(args ...any) {
	l.log(ErrorLevel, args...)
}

func (l *Logger) Errorf(f string, args ...interface{}) {
	l.logf(ErrorLevel, f, args...)
}

func (l *Logger) Fatal(args ...any) {
	l.log(FatalLevel, args...)
}

func (l *Logger) Fatalf(f string, args ...interface{}) {
	l.logf(FatalLevel, f, args...)
}
