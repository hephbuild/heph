package hlog

import (
	"log/slog"
)

type Logger = *slog.Logger

func NewLogger(h slog.Handler) Logger {
	return slog.New(h)
}
