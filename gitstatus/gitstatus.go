package gitstatus

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/ads"
	"golang.org/x/exp/slices"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
)

type GitStatus struct {
	root string
	m    sync.Mutex

	dirty []string
}

func New(root string) *GitStatus {
	return &GitStatus{root: root}
}

func (gs *GitStatus) diffIndexOnce(ctx context.Context) []string {
	gs.m.Lock()
	defer gs.m.Unlock()

	if gs.dirty == nil {
		gs.dirty = gs.diffIndex(ctx)
		if gs.dirty == nil {
			gs.dirty = []string{}
		}
	}

	return gs.dirty[:]
}

func (gs *GitStatus) diffIndex(ctx context.Context) []string {
	gitPath, err := exec.LookPath("git")
	if err != nil {
		// git not found, assume nothing is dirty
		return nil
	}

	cmd := exec.CommandContext(ctx, gitPath, "diff-index", "HEAD", "--name-only")
	cmd.Dir = gs.root

	b, err := cmd.Output()
	if err != nil {
		// something failed, assume nothing is dirty
		log.Errorf("git: diff-index: %v", err)
		return nil
	}

	lines := strings.Split(string(b), "\n")
	lines = ads.Filter(lines, func(s string) bool {
		return s != ""
	})

	return ads.Map(lines, func(line string) string {
		return filepath.Join(gs.root, line)
	})
}

func (gs *GitStatus) IsDirty(ctx context.Context, path string) bool {
	dirty := gs.diffIndexOnce(ctx)

	return slices.Contains(dirty, path)
}

func (gs *GitStatus) Reset() {
	gs.m.Lock()
	defer gs.m.Unlock()

	gs.dirty = nil
}
