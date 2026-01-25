package hcobra

import (
	"strconv"

	"github.com/spf13/pflag"
)

func NewBoolStrFlag(bs *BoolStr, name, shorthand, usage string) *pflag.Flag {
	flag := &pflag.Flag{
		Name:        name,
		Shorthand:   shorthand,
		Usage:       usage,
		Value:       bs,
		DefValue:    "false",
		NoOptDefVal: "true",
	}

	return flag
}

type BoolStr struct {
	Bool bool
	Str  string
}

func (bs *BoolStr) Set(s string) error {
	b, err := strconv.ParseBool(s)
	if err == nil {
		s = ""
	} else {
		b = true
	}

	*bs = BoolStr{
		Bool: b,
		Str:  s,
	}

	return nil
}

func (bs *BoolStr) Type() string {
	return "bool|str"
}

func (bs *BoolStr) String() string {
	if bs.Bool {
		return bs.Str
	}

	return "false"
}
