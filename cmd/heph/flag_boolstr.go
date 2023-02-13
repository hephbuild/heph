package main

import "github.com/spf13/pflag"

func NewBoolStrFlag(bs *boolStr, name, shorthand, usage string) *pflag.Flag {
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

type boolStr struct {
	bool bool
	str  string
}

func (bs *boolStr) Set(s string) error {
	if s == "true" {
		s = ""
	}

	*bs = boolStr{
		bool: true,
		str:  s,
	}

	return nil
}

func (bs *boolStr) Type() string {
	return "bool|str"
}

func (bs *boolStr) String() string {
	if bs.bool {
		return bs.str
	}

	return "false"
}
