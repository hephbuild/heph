package main

import (
	"golang.org/x/exp/errors"
	"golang.org/x/exp/maps"
)

func main() {
	// maps is a package of module: golang.org/x/exp
	maps.Clear(map[string]string{})
	// errors is a module: golang.org/x/exp/errors
	_ = errors.New("foo")
}
