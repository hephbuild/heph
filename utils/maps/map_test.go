package maps

import (
	"github.com/stretchr/testify/assert"
	"testing"
	"time"
)

func TestExpiration(t *testing.T) {
	m := Map[int, string]{
		Default: func(k int) string {
			return time.Now().String()
		},
		Expiration: time.Second,
	}

	v1 := m.Get(1)
	assert.Equal(t, 1, len(m.Raw()))
	v2 := m.Get(1)
	assert.Equal(t, 1, len(m.Raw()))
	assert.Equal(t, v1, v2)

	time.Sleep(2 * time.Second)

	assert.Equal(t, 0, len(m.Raw()))

	v3 := m.Get(1)
	assert.NotEqual(t, v1, v3)
}
