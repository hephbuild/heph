package main

import "testing"

func TestDiskNoticeFiresOncePerThreshold(t *testing.T) {
	dir := t.TempDir()
	const gb = 1 << 30
	for i, step := range []struct {
		free uint64
		want bool
	}{
		{100 * gb, false},
		{29 * gb, true},  // crossed 30
		{25 * gb, false}, // still between 30 and 10: already said
		{9 * gb, true},   // crossed 10
		{2 * gb, false},
		{50 * gb, false}, // cleaned up: re-arms
		{20 * gb, true},
	} {
		if got := diskNotice(step.free, "s", dir) != ""; got != step.want {
			t.Errorf("step %d (%d GB free): notice %v, want %v", i, step.free/gb, got, step.want)
		}
	}
	// Another session or subagent has its own state.
	if diskNotice(20*gb, "other", dir) == "" {
		t.Error("a new key should be told")
	}
}

func TestFreeBytesReadsTheVolume(t *testing.T) {
	if free, ok := freeBytes(t.TempDir()); !ok || free == 0 {
		t.Errorf("freeBytes = %d, %v", free, ok)
	}
	if _, ok := freeBytes(""); ok {
		t.Error("no path should be unknown, not 0 free")
	}
}
