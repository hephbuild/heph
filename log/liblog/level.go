package liblog

import (
	"fmt"
	"strings"
)

type Level int8

func ParseLevel(s string) (Level, error) {
	ls := strings.ToLower(s)

	for lvl := _minLevel; lvl <= _maxLevel; lvl++ {
		if strings.ToLower(lvl.String()) == ls {
			return lvl, nil
		}
	}

	switch ls {
	case "trace":
		return TraceLevel, nil
	case "debug":
		return DebugLevel, nil
	case "info":
		return InfoLevel, nil
	case "error":
		return ErrorLevel, nil
	case "panic":
		return PanicLevel, nil
	case "fatal":
		return FatalLevel, nil
	}

	return InvalidLevel, fmt.Errorf("invalid level %v", s)
}

func (l Level) String() string {
	switch l {
	case TraceLevel:
		return "TRC"
	case DebugLevel:
		return "DBG"
	case InfoLevel:
		return "INF"
	case WarnLevel:
		return "WRN"
	case ErrorLevel:
		return "ERR"
	case PanicLevel:
		return "PNC"
	case FatalLevel:
		return "FAT"
	default:
		return "INV"
	}
}

const (
	InvalidLevel Level = 0
	TraceLevel   Level = iota
	DebugLevel
	InfoLevel
	WarnLevel
	ErrorLevel
	PanicLevel
	// FatalLevel logs a message, then calls os.Exit(1).
	FatalLevel

	_minLevel = DebugLevel
	_maxLevel = FatalLevel
)
