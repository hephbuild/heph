package main

import (
	"encoding/json"
	"os"

	"github.com/hephbuild/heph/lib/tref/internal/lexer"
)

func main() {
	err := json.NewEncoder(os.Stdout).Encode(lexer.GetLexer())
	if err != nil {
		panic(err)
	}
}
