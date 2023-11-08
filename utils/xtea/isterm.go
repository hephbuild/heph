package xtea

import (
	"github.com/hephbuild/heph/log/log"
	"github.com/mattn/go-isatty"
)

var isTerm bool

func init() {
	if w := log.Writer(); w != nil {
		isTerm = isatty.IsTerminal(w.Fd())
	}
}

func IsTerm() bool {
	return isTerm
}
