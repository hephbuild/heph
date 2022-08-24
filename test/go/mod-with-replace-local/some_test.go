package main

import (
	"mod-simple/hello"
	"testing"
)

func TestSanity(t *testing.T) {
	t.Log("Main - TestSanity Hello world")

	hello.Hello()
}
