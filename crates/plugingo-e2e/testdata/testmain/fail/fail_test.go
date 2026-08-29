package fail

import "testing"

// Same TestMain shape as the `pass` package. Note it does *not* call os.Exit:
// the exit code has to travel out through M's `exitCode` field, which is
// exactly the mechanism under test.
func TestMain(m *testing.M) {
	m.Run()
}

func TestSumIsDeliberatelyWrong(t *testing.T) {
	if got := Sum(1, 2); got != 3 {
		t.Fatalf("Sum(1, 2) = %d, want 3", got)
	}
}
