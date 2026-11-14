package main

// The low-disk notice. A full disk stopped five of the six go and bug-fix
// sessions in the week to 2026-11-05: one sat blocked for four hours, fix
// agents handed back half-done work after ENOSPC, and an e2e failure had to be
// argued into "environmental" each time. Nothing said the disk was filling
// until a build failed. Like the context notice, this tells the model once per
// threshold crossed and allows the command either way.

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"syscall"
)

// Free space under which the notice fires, highest first. A cold heph
// worktree's target/ is 20-45 GB; below the first there is room for one more.
var diskWarnAt = []uint64{30 << 30, 10 << 30}

const diskNoticeText = "Disk: %d GB free on this volume. Below ~5 GB builds fail with `No space left on device` (os error 28), " +
	"agents hand back half-done work, and every command can end up refused — that is environmental, not a regression to debug. " +
	"The space usually goes to each worktree's cargo `target/` (20-45 GB each), the kache store, and `.heph/` sandboxes. " +
	"`kache clean --orphans -y` (targets of deleted worktrees) is always safe; `kache clean --stale 2d --dry-run` previews more. " +
	"Ask the user before removing anything else: another worktree's tree or target, a running session's sandbox, the kache store. " +
	"See CLAUDE.md \"Disk\"."

// diskNotice returns a reminder when free has dropped past the next threshold
// since the last one reported under key, or "". Rising back above a threshold
// re-arms it, so a cleanup followed by a refill warns again.
func diskNotice(free uint64, key, stateDir string) string {
	level := 0
	for _, at := range diskWarnAt {
		if free < at {
			level++
		}
	}
	state := filepath.Join(stateDir, "heph-gate-disk-"+filepath.Base(key))
	reported := 0
	if b, err := os.ReadFile(state); err == nil {
		reported, _ = strconv.Atoi(string(b))
	}
	if level != reported {
		_ = os.WriteFile(state, []byte(strconv.Itoa(level)), 0o600)
	}
	if level == 0 || level <= reported {
		return ""
	}
	return fmt.Sprintf(diskNoticeText, free>>30)
}

// freeBytes is the space an unprivileged process can still write under path,
// or false when it cannot be read.
func freeBytes(path string) (uint64, bool) {
	var st syscall.Statfs_t
	if path == "" || syscall.Statfs(path, &st) != nil {
		return 0, false
	}
	// Bsize is int64 on Linux and uint32 on macOS.
	return st.Bavail * uint64(st.Bsize), true
}
