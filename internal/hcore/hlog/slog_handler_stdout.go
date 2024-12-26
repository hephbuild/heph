package hlog

import (
	"log/slog"
	"os"
)

func NewStdoutLogger(leveler slog.Leveler) *slog.Logger {
	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
		AddSource: false,
		Level:     leveler,
	}))

	return logger
}
