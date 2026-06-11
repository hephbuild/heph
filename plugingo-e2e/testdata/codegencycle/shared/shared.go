package shared

import "codegencycle/genpb"

// Msg is imported by both binaries, so shared:build_lib is resolved
// concurrently while a sibling package's go_src query enumerates it.
func Msg() string {
	return genpb.GeneratedVar
}
