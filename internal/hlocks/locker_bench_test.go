package hlocks

import (
	"context"
	"testing"
)

func BenchmarkFlock(b *testing.B) {
	fs := newfs(b)

	l := NewFlock(fs, "", "a.lock")

	b.ResetTimer()

	for b.Loop() {
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

	for b.Loop() {
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
