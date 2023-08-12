package xreflow

import (
	"bytes"
	"github.com/muesli/reflow/ansi"
	"github.com/muesli/reflow/truncate"
	"github.com/muesli/reflow/wrap"
	"strings"
)

func WrapString(s string, limit int) string {
	ws := wrap.String(s, limit)

	if ws == s {
		return s
	}

	var buf bytes.Buffer
	w := ansi.Writer{Forward: &buf}

	lines := strings.Split(ws, "\n")
	for i, line := range lines {
		w.RestoreAnsi()
		_, _ = w.Write([]byte(line))
		w.ResetAnsi()
		if i < len(lines)-1 {
			buf.WriteString("\n")
		}
	}

	return buf.String()
}

func TruncateString(s string, limit uint) string {
	ts := truncate.String(s, limit)

	if ts == s {
		return s
	}

	var buf bytes.Buffer
	w := ansi.Writer{Forward: &buf}
	_, _ = w.Write([]byte(ts))
	w.ResetAnsi()

	return buf.String()
}
