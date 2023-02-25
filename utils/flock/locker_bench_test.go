package flock

import (
	"context"
	"github.com/stretchr/testify/require"
	"os"
	"path/filepath"
	"testing"
)

func BenchmarkFlock(b *testing.B) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(b, err)
	defer os.RemoveAll(dir)

	l := NewFlock("", filepath.Join(dir, "a.lock"))

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		err := l.Lock(context.Background())
		if err != nil {
			panic(err)
		}
		err = l.Unlock()
		if err != nil {
			panic(err)
		}
	}
}

func BenchmarkMutex(b *testing.B) {
	l := NewMutex("")

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		err := l.Lock(context.Background())
		if err != nil {
			panic(err)
		}
		err = l.Unlock()
		if err != nil {
			panic(err)
		}
	}
}
