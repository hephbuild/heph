//go:build release

package hversion

const version = ""

func Version() string {
	if version == "" {
		panic("Version is empty")
	}

	return version
}
