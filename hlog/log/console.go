package log

import (
	"bytes"
	"github.com/charmbracelet/lipgloss"
	"io"
	"sync"
)

var LevelColors = map[Level]lipgloss.TerminalColor{
	TraceLevel: lipgloss.Color("#3FCD23"),
	DebugLevel: lipgloss.Color("#29C6E8"),
	InfoLevel:  lipgloss.Color("#2C75FE"),
	WarnLevel:  lipgloss.Color("#E7C229"),
	ErrorLevel: lipgloss.Color("#FF2A25"),
	PanicLevel: lipgloss.Color("#FF2A25"),
	FatalLevel: lipgloss.Color("#FF2A25"),
}

var LevelStyles map[Level]lipgloss.Style

func init() {
	LevelStyles = map[Level]lipgloss.Style{}
	for lvl, color := range LevelColors {
		LevelStyles[lvl] = lipgloss.NewStyle().Bold(true).Foreground(color)
	}
}

func LevelStyle(lvl Level) lipgloss.Style {
	if style, ok := LevelStyles[lvl]; ok {
		return style
	}

	return lipgloss.Style{}
}

func NewConsoleFormatter(w io.Writer) *Formatter {
	// TODO: figure out how to make the colors per write ColorProfile
	//colorProfile := termenv.NewOutput(w).ColorProfile()

	return &Formatter{}
}

type Formatter struct{}

var fmtBufPool = sync.Pool{New: func() any {
	return new(bytes.Buffer)
}}

func (f *Formatter) Format(entry Entry) []byte {
	buf := fmtBufPool.Get().(*bytes.Buffer)
	buf.Reset()
	defer fmtBufPool.Put(buf)

	buf.WriteString(LevelStyle(entry.Level).Render(entry.Level.String() + "|"))
	buf.WriteRune(' ')
	buf.WriteString(entry.Message)

	return buf.Bytes()
}

func NewConsole(w io.Writer) Collector {
	return console{w: w, fmt: NewConsoleFormatter(w)}
}

type console struct {
	w   io.Writer
	fmt *Formatter
}

func (c console) Write(entry Entry) error {
	c.w.Write(c.fmt.Format(entry))
	c.w.Write([]byte{'\n'})
	return nil
}
