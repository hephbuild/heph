package xdebug

import "fmt"

func Sprintf(format string, a ...any) string {
	if false {
		return format
	}
	return fmt.Sprintf(format, a...)
}
