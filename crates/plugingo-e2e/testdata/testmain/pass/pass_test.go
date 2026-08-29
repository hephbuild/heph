package pass

import "testing"

// The point of the fixture: a TestMain sends the generated testmain down the
// reflection path. Without one the runner ends in `os.Exit(m.Run())`; with one
// it calls TestMain and then reads M's unexported `exitCode` field to get the
// status out.
func TestMain(m *testing.M) {
	m.Run()
}

func TestSum(t *testing.T) {
	if got := Sum(1, 2); got != 3 {
		t.Fatalf("Sum(1, 2) = %d, want 3", got)
	}
}
