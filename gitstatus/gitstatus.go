package gitstatus

import (
	"context"
	"github.com/hephbuild/heph/utils/ads"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
)

type GitStatus struct {
	root string
	o    sync.Once

	dirty map[string]struct{}
}

func New(root string) *GitStatus {
	return &GitStatus{root: root}
}

func (gs *GitStatus) diffIndexOnce(ctx context.Context) map[string]struct{} {
	gs.o.Do(func() {
		// If something went wrong, we can assume nothing is dirty
		dirty, _ := gs.diffIndex(ctx)

		gs.dirty = make(map[string]struct{}, len(dirty))
		for _, file := range dirty {
			gs.dirty[file] = struct{}{}
		}
	})

	return gs.dirty
}

func (gs *GitStatus) diffIndex(ctx context.Context) ([]string, error) {
	gitPath, err := exec.LookPath("git")
	if err != nil {
		return nil, err
	}

	cmd := exec.CommandContext(ctx, gitPath, "diff-index", "HEAD", "--name-only")
	cmd.Dir = gs.root

	b, err := cmd.Output()
	if err != nil {
		return nil, err
	}

	lines := strings.Split(string(b), "\n")
	lines = ads.Filter(lines, func(s string) bool {
		return s != ""
	})

	return ads.Map(lines, func(line string) string {
		return filepath.Join(gs.root, line)
	}), nil
}

func (gs *GitStatus) IsDirty(ctx context.Context, path string) bool {
	dirty := gs.diffIndexOnce(ctx)

	_, ok := dirty[path]
	return ok
}

func (gs *GitStatus) Reset() {
	gs.o = sync.Once{}
}
