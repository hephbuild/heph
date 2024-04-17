package xdebug

import "fmt"

const enabled = true

// Sprintf is fmt.Sprintf, but zero-allocation/overhead when debug is disabled
func Sprintf(format string, a ...any) string {
	if enabled {
		return fmt.Sprintf(format, a...)
	}
	return format
}
