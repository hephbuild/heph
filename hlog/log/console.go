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

type Buffer struct {
	buf *bytes.Buffer
}

func (b Buffer) Bytes() []byte {
	return b.buf.Bytes()
}

func (b Buffer) Free() {
	fmtBufPool.Put(b.buf)
}

func (f *Formatter) Format(entry Entry) Buffer {
	buf := fmtBufPool.Get().(*bytes.Buffer)
	buf.Reset()

	buf.WriteString(LevelStyle(entry.Level).Render(entry.Level.String() + "|"))
	buf.WriteRune(' ')
	buf.WriteString(entry.Message)

	return Buffer{buf}
}

func NewConsole(w io.Writer) Collector {
	return console{w: w, fmt: NewConsoleFormatter(w)}
}

type console struct {
	w   io.Writer
	fmt *Formatter
}

func (c console) Write(entry Entry) error {
	buf := c.fmt.Format(entry)
	defer buf.Free()

	c.w.Write(buf.Bytes())
	c.w.Write([]byte{'\n'})
	return nil
}
