package log

import "time"

type Entry struct {
	Timestamp time.Time
	Level     Level
	Message   string
}
