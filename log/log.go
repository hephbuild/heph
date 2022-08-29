package log

import (
	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
	"os"
)

var std Logger

var printfFunc PrintfFunc

func SetPrintfFunc(fn PrintfFunc) {
	printfFunc = fn
}

var level = zap.NewAtomicLevel()
var traceEnabled = false

func Init(isTerm bool) error {
	level.SetLevel(InfoLevel)

	consoleEncoderConfig := zapcore.EncoderConfig{
		LevelKey:         "level",
		MessageKey:       "msg",
		LineEnding:       zapcore.DefaultLineEnding,
		EncodeLevel:      zapcore.CapitalLevelEncoder,
		ConsoleSeparator: " ",
	}
	if isTerm {
		consoleEncoderConfig.EncodeLevel = zapcore.CapitalColorLevelEncoder
	}
	consoleEncoder := zapcore.NewConsoleEncoder(consoleEncoderConfig)

	stderr := zapcore.Lock(os.Stderr)

	core := zapcore.NewTee(
		NewFilterCore(zapcore.NewCore(consoleEncoder, stderr, level), func(zapcore.Entry) bool {
			return printfFunc == nil
		}), // Console
		NewPrintfCore(consoleEncoder, level, &printfFunc), // bubbletea
	)

	logger := zap.New(core)
	std = logger.Sugar()

	return nil
}

func Cleanup() error {
	if std == nil {
		return nil
	}

	return std.Sync()
}
