package main

import (
	"encoding/json"
	"gobackend/embed"
	"os"
)

func genEmbed() {
	files := os.Args[1:]

	cfg, err := embed.Parse(files)
	if err != nil {
		panic(err)
	}

	b, err := json.Marshal(cfg)
	if err != nil {
		panic(err)
	}
	os.Stdout.Write(b)
}
