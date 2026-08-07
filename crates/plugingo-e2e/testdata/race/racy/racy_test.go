package racy

import "testing"

func TestIncrement(t *testing.T) {
	// Passes without instrumentation — the assertion is deliberately loose, so
	// the only thing that can fail this test is the race detector itself.
	if got := Increment(); got < 1 {
		t.Fatalf("Increment() = %d, want >= 1", got)
	}
}
