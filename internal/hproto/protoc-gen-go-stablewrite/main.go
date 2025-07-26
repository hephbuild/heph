package main

import (
	"flag"
	"google.golang.org/protobuf/compiler/protogen"
)

var (
	flags flag.FlagSet
)

func main() {
	protogen.Options{ParamFunc: flags.Set}.Run(Generate)
}
