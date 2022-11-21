package main

import (
	"github.com/stretchr/testify/assert"
	"os"
	"testing"
)

func TestEnv(t *testing.T) {
	t.Log("Ran child3")
	assert.Equal(t, "42", os.Getenv("CHILD3_MAGIC_VALUE1"))
	assert.Equal(t, "42", os.Getenv("CHILD3_MAGIC_VALUE2"))
	assert.Equal(t, "42", os.Getenv("CHILD3_MAGIC_VALUE3"))
}
