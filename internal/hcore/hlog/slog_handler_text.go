package hlog

import (
	"context"
	"fmt"
	"image/color"
	"io"
	"log/slog"
	"strings"

	"charm.land/lipgloss/v2"
	"github.com/hephbuild/heph/internal/hlipgloss"
)

var levelColors = map[slog.Level]color.Color{
	slog.LevelDebug: lipgloss.Color("#29C6E8"),
	slog.LevelInfo:  lipgloss.Color("#2C75FE"),
	slog.LevelWarn:  lipgloss.Color("#E7C229"),
	slog.LevelError: lipgloss.Color("#FF2A25"),
}

var levelStyles = map[slog.Level]lipgloss.Style{}

func init() {
	for lvl, c := range levelColors {
		levelStyles[lvl] = lipgloss.NewStyle().Bold(true).Foreground(c)
	}
}

type textHandler struct {
	attrs   []slog.Attr
	leveler slog.Leveler
	w       io.Writer
}

func (t textHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return level >= t.leveler.Level()
}

func FormatRecord(attrs []slog.Attr, record slog.Record) string {
	var sb strings.Builder
	sb.WriteString(levelStyles[record.Level].Render(record.Level.String()))
	sb.WriteString(" ")
	sb.WriteString(record.Message)

	renderAttr := func(attr slog.Attr) {
		sb.WriteString(" ")
		sb.WriteString(attr.Key)
		sb.WriteString("=")
		sb.WriteString(strings.ReplaceAll(fmt.Sprint(attr.Value.Any()), "\n", `\n`))
	}

	for _, attr := range attrs {
		renderAttr(attr)
	}

	record.Attrs(func(attr slog.Attr) bool {
		renderAttr(attr)

		return true
	})

	return sb.String()
}

func (t textHandler) Handle(ctx context.Context, record slog.Record) error {
	_, err := t.w.Write([]byte(FormatRecord(t.attrs, record)))
	if err != nil {
		return err
	}
	_, err = t.w.Write([]byte("\r\n"))
	if err != nil {
		return err
	}
	return nil
}

func (t textHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	t.attrs = append(t.attrs, attrs...)

	return t
}

func (t textHandler) WithGroup(name string) slog.Handler {
	return t
}

func NewTextLogger(w io.Writer, leveler slog.Leveler) Logger {
	return NewLogger(textHandler{
		w:       hlipgloss.NewWriter(w),
		leveler: leveler,
	})
}
