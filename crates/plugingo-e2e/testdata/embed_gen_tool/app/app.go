package app

import _ "embed"

//go:embed openapi/openapi_gen.yaml
var Spec string

// Spec returns the embedded generated asset.
func Get() string { return Spec }
