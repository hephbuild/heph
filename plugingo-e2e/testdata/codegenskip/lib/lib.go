package lib

import v1 "codegenskip/gen/pb/v1"

// Greeting imports the generated package, forcing the go provider to resolve
// `//gen/pb/v1:_golist` for the importer's `go list`.
func Greeting() string {
	return v1.GeneratedVar
}
