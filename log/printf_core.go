package log

import (
	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
	"strings"
)

type PrintfFunc func(template string, args ...interface{})

func NewPrintfCore(enc zapcore.Encoder, enab zapcore.LevelEnabler, p *PrintfFunc) zapcore.Core {
	return &printfCore{
		printf: p,
		enab:   enab,
		enc:    enc,
	}
}

type printfCore struct {
	printf *PrintfFunc
	enc    zapcore.Encoder
	enab   zapcore.LevelEnabler
}

func (c *printfCore) Enabled(l zapcore.Level) bool {
	return c.enab.Enabled(l)
}

func (c *printfCore) With(fields []zapcore.Field) zapcore.Core {
	return c
}

func (c *printfCore) Check(e zapcore.Entry, ce *zapcore.CheckedEntry) *zapcore.CheckedEntry {
	fn := *c.printf
	if fn != nil && c.Enabled(e.Level) {
		return ce.AddCore(e, c)
	}

	return ce
}

func (c *printfCore) Sync() error {
	return nil
}

func (c *printfCore) doPrintf(f PrintfFunc, e zapcore.Entry, fields []zap.Field) error {
	b, err := c.enc.EncodeEntry(e, fields)
	if err != nil {
		return err
	}

	s := strings.TrimSpace(b.String())
	b.Free()

	f("%s", s)

	return nil
}

func (c *printfCore) Write(e zapcore.Entry, fields []zap.Field) error {
	fn := *c.printf

	return c.doPrintf(fn, e, fields)
}
