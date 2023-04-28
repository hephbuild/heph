package liblog

import (
	"bytes"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/utils/xlipgloss"
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

func NewConsoleFormatter(w io.Writer) *ConsoleFormatter {
	r := lipgloss.NewRenderer(w, xlipgloss.EnvForceTTY())

	lvlStyles := map[Level]lipgloss.Style{}
	for lvl, color := range LevelColors {
		lvlStyles[lvl] = r.NewStyle().Bold(true).Foreground(color)
	}

	return &ConsoleFormatter{
		lvlStyles:      lvlStyles,
		componentStyle: r.NewStyle().Bold(true).Foreground(lipgloss.Color("#FF8825")),
		reqidStyle:     r.NewStyle().Bold(true),
	}
}

type ConsoleFormatter struct {
	lvlStyles      map[Level]lipgloss.Style
	componentStyle lipgloss.Style
	reqidStyle     lipgloss.Style
}

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

func (f *ConsoleFormatter) lvlStyle(lvl Level) lipgloss.Style {
	if style, ok := f.lvlStyles[lvl]; ok {
		return style
	}

	return lipgloss.Style{}
}

const componentKey = "component"
const reqidKey = "req_id"

func (f *ConsoleFormatter) Format(entry Entry) Buffer {
	buf := fmtBufPool.Get().(*bytes.Buffer)
	buf.Reset()

	buf.WriteString(f.lvlStyle(entry.Level).Render(entry.Level.String() + "|"))
	buf.WriteRune(' ')

	for _, field := range entry.Fields {
		if field.Key == componentKey || field.Key == reqidKey {
			style := lipgloss.Style{}
			switch field.Key {
			case componentKey:
				style = f.componentStyle
			case reqidKey:
				style = f.reqidStyle
			}
			var value bytes.Buffer
			field.Value.Write(&value)
			buf.WriteString(style.Render("[" + value.String() + "]"))
			buf.WriteRune(' ')
		}
	}

	buf.WriteString(entry.Message)
	tabbed := false
	for _, f := range entry.Fields {
		if f.Key == componentKey {
			continue
		}

		if f.Key == reqidKey {
			continue
		}

		if !tabbed {
			buf.WriteString("\t")
			tabbed = true
		} else {
			buf.WriteRune(' ')
		}
		buf.WriteString(f.Key)
		buf.WriteRune('=')
		f.Value.Write(buf)
	}

	return Buffer{buf}
}

func NewConsole(w io.Writer) Collector {
	return console{w: w, fmt: NewConsoleFormatter(w)}
}

func NewConsoleJSON(w io.Writer) Collector {
	return console{w: w, fmt: NewJSONFormatter()}
}

type Formatter interface {
	Format(entry Entry) Buffer
}

type console struct {
	w   io.Writer
	fmt Formatter
}

func (c console) Write(entry Entry) error {
	buf := c.fmt.Format(entry)
	defer buf.Free()

	c.w.Write(buf.Bytes())
	c.w.Write([]byte{'\n'})
	return nil
}
