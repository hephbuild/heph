package liblog

import (
	"bytes"
	"encoding/json"
	"fmt"
	"time"
)

func NewJSONFormatter() *JSONFormatter {
	return &JSONFormatter{
		MsgField:       "message",
		LevelField:     "severity",
		TimestampField: "time",
	}
}

type JSONFormatter struct {
	MsgField       string
	LevelField     string
	TimestampField string
}

func (j *JSONFormatter) level(lvl Level) string {
	switch lvl {
	case TraceLevel:
		return "DEBUG"
	case DebugLevel:
		return "DEBUG"
	case InfoLevel:
		return "INFO"
	case WarnLevel:
		return "WARNING"
	case ErrorLevel:
		return "ERROR"
	case PanicLevel:
		return "CRITICAL"
	case FatalLevel:
		return "CRITICAL"
	default:
		return lvl.String()
	}
}

func (j *JSONFormatter) Format(entry Entry) Buffer {
	buf := fmtBufPool.Get()
	buf.Reset()

	m := map[string]string{
		j.MsgField:       entry.Message,
		j.LevelField:     j.level(entry.Level),
		j.TimestampField: entry.Timestamp.Format(time.RFC3339Nano),
	}
	var vbuf bytes.Buffer
	for _, f := range entry.Fields {
		vbuf.Reset()
		f.Value.Write(&vbuf)
		m[f.Key] = vbuf.String()
	}

	enc := json.NewEncoder(buf)
	err := enc.Encode(m)
	if err != nil {
		fmt.Println("JSON FORMATTER ERROR", err)
	}

	if buf.Len() > 0 {
		buf.Truncate(buf.Len() - 1)
	}

	return Buffer{buf}
}
