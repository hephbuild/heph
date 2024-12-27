package hlog

import (
	"log/slog"
	"os"
)

func NewStdoutLogger(leveler slog.Leveler) Logger {
	return NewLogger(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
		AddSource: false,
		Level:     leveler,
	}))
}
