package hlog

import (
	"context"
	"io"
	"log/slog"
	"strings"

	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/internal/hlipgloss"
)

var levelColors = map[slog.Level]lipgloss.TerminalColor{
	slog.LevelDebug: lipgloss.Color("#29C6E8"),
	slog.LevelInfo:  lipgloss.Color("#2C75FE"),
	slog.LevelWarn:  lipgloss.Color("#E7C229"),
	slog.LevelError: lipgloss.Color("#FF2A25"),
}

type textHandler struct {
	attrs   []slog.Attr
	leveler slog.Leveler
	w       io.Writer

	renderer Renderer
}

func (t textHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return level >= t.leveler.Level()
}

func FormatRecord(r Renderer, record slog.Record) string {
	var sb strings.Builder
	sb.WriteString(r.lvlStyles[record.Level].Render(record.Level.String()))
	sb.WriteString(" ")
	sb.WriteString(record.Message)

	return sb.String()
}

func (t textHandler) Handle(ctx context.Context, record slog.Record) error {
	_, err := t.w.Write([]byte(FormatRecord(t.renderer, record)))
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

type Renderer struct {
	lvlStyles map[slog.Level]lipgloss.Style
}

func NewRenderer(w io.Writer) Renderer {
	r := hlipgloss.NewRenderer(w)

	lvlStyles := map[slog.Level]lipgloss.Style{}
	for lvl, color := range levelColors {
		lvlStyles[lvl] = r.NewStyle().Bold(true).Foreground(color)
	}

	return Renderer{lvlStyles: lvlStyles}
}

func NewTextLogger(w io.Writer, leveler slog.Leveler) Logger {
	return NewLogger(textHandler{
		w:        w,
		leveler:  leveler,
		renderer: NewRenderer(w),
	})
}
