package main

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// The file devenv.nix's `lint` writes into the git dir on success: the tree it
// checked, working-tree changes included.
const lintStampFile = "heph-lint-ok"

const unlintedReason = "Blocked: %s.\n" +
	"A push that `lint` would have failed costs a full CI round-trip to find out. Run `lint` and branch on its exit code " +
	"(rustfmt prints `Diff in`, not `error`), commit, then push.\n" +
	"`lint` from a devenv shell started before this gate existed does not write the stamp — start a new `devenv shell`.\n" +
	"If this push should go out unlinted (a WIP branch nobody reviews yet), re-run as `HEPH_PUSH_UNLINTED=1 git push ...`."

// lintStamp says why HEAD in dir should not be pushed yet, or "" when the last
// passing `lint` there checked exactly HEAD's tree. Anything git cannot answer
// — not a repository, no commits — allows.
func lintStamp(dir string) string {
	if dir == "" {
		dir = "."
	}
	gitDir, err := gitOut(dir, "rev-parse", "--absolute-git-dir")
	if err != nil {
		return ""
	}
	head, err := gitOut(dir, "rev-parse", "HEAD^{tree}")
	if err != nil {
		return ""
	}
	stamp, err := os.ReadFile(filepath.Join(gitDir, lintStampFile))
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return fmt.Sprintf(unlintedReason, "`lint` has not passed in this checkout")
	case err != nil:
		return ""
	case strings.TrimSpace(string(stamp)) == head:
		return ""
	}
	return fmt.Sprintf(unlintedReason, "HEAD is not the tree `lint` last passed on — something was committed or edited after it ran")
}

func gitOut(dir string, args ...string) (string, error) {
	out, err := exec.Command("git", append([]string{"-C", dir}, args...)...).Output()
	return strings.TrimSpace(string(out)), err
}
