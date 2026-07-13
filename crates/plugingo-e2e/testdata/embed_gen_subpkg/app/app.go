package app

import _ "embed"

//go:embed openapi/openapi_gen.yaml
var spec string

// Get returns the embedded generated asset.
func Get() string { return spec }
