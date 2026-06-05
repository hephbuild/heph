//go:build never_built_tag

package lib

import "fmt"

func Greet(name string) string {
	return fmt.Sprintf("hello, %s", name)
}
