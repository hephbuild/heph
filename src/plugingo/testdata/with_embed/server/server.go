package server

import _ "embed"

//go:embed static/index.html
var indexHTML []byte

func IndexHTML() []byte {
	return indexHTML
}
