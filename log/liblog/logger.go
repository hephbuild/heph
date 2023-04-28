package liblog

import (
	"fmt"
	"os"
	"strings"
	"time"
)

func NewLogger(core Core) Logger {
	return Logger{core: core}
}

type Logger struct {
	core   Core
	fields []EntryField
}

func (l Logger) logf(lvl Level, f string, args ...interface{}) {
	if !l.core.Enabled(lvl) {
		return
	}

	l.logs(lvl, fmt.Sprintf(f, args...))
}

func (l Logger) log(lvl Level, args ...any) {
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

	l.logs(lvl, strings.Join(argsstr, " "))
}
func (l Logger) logs(lvl Level, s string) {
	if !l.core.Enabled(lvl) {
		return
	}

	err := l.core.Log(Entry{
		Timestamp: time.Now(),
		Level:     lvl,
		Message:   s,
		Fields:    l.fields,
	})
	if err != nil {
		_, _ = fmt.Fprintf(os.Stderr, "error logging: %v", err)
	}
}

func (l Logger) Trace(args ...any) {
	l.log(TraceLevel, args...)
}

func (l Logger) Tracef(f string, args ...interface{}) {
	l.logf(TraceLevel, f, args...)
}

func (l Logger) Debug(args ...any) {
	l.log(DebugLevel, args...)
}

func (l Logger) Debugf(f string, args ...interface{}) {
	l.logf(DebugLevel, f, args...)
}

func (l Logger) Info(args ...any) {
	l.log(InfoLevel, args...)
}

func (l Logger) Infof(f string, args ...interface{}) {
	l.logf(InfoLevel, f, args...)
}

func (l Logger) Warn(args ...any) {
	l.log(WarnLevel, args...)
}

func (l Logger) Warnf(f string, args ...interface{}) {
	l.logf(WarnLevel, f, args...)
}

func (l Logger) Error(args ...any) {
	l.log(ErrorLevel, args...)
}

func (l Logger) Errorf(f string, args ...interface{}) {
	l.logf(ErrorLevel, f, args...)
}

func (l Logger) Fatal(args ...any) {
	l.log(FatalLevel, args...)
	os.Exit(1)
}

func (l Logger) Fatalf(f string, args ...interface{}) {
	l.logf(FatalLevel, f, args...)
	os.Exit(1)
}

func (l Logger) Fields(args ...interface{}) Logger {
	if len(args)%2 != 0 {
		panic("k/v pairs length must be even")
	}

	nfields := l.fields
	var k string
	for i, arg := range args {
		if i%2 == 0 {
			k = arg.(string)
			continue
		}

		var value FieldMarshal
		switch arg := arg.(type) {
		case string:
			value = StringMarshal(arg)
		case fmt.Stringer:
			value = StringMarshal(arg.String())
		default:
			value = FallbackMarshal(arg)
		}

		nfields = append(nfields, EntryField{
			Key:   k,
			Value: value,
		})
	}
	l.fields = nfields
	return l
}

func (l Logger) IsLevelEnabled(lvl Level) bool {
	return l.core.Enabled(lvl)
}
