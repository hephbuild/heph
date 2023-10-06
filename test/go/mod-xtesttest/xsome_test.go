package xtest_test

import (
	"github.com/stretchr/testify/assert"
	xtest "mod-xtesttest"
	"testing"
)

func TestSanity(t *testing.T) {
	xtest.Hello()

	// :)
	assert.Equal(t, 1, 1)
}
