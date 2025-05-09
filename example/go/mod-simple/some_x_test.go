package main_test

import (
	"mod-simple/hello"
	"testing"
)

func TestSanity(t *testing.T) {
	hello.Hello()

	t.Log("Main - TestSanity Hello world")
}
