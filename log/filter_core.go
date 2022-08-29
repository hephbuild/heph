package log

import "go.uber.org/zap/zapcore"

func NewFilterCore(c zapcore.Core, filter func(zapcore.Entry) bool) zapcore.Core {
	return &filterCore{
		Core:   c,
		filter: filter,
	}
}

type filterCore struct {
	zapcore.Core
	filter func(zapcore.Entry) bool
}

func (f *filterCore) Check(e zapcore.Entry, ce *zapcore.CheckedEntry) *zapcore.CheckedEntry {
	if f.filter(e) {
		return f.Core.Check(e, ce)
	}

	return ce
}
