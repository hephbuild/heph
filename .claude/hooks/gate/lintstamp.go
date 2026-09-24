package main

// The lint stamp: which tree `lint` last passed on, so a push of any other
// tree can be refused. It lives entirely here, not in `lint` itself — `lint`
// is what CI runs, and CI has no use for it.
//
// The hook rewrites every `lint` an agent's command runs into a call back to
// this program (stampLint), which records the tree, runs the real `lint`, and
// writes the stamp only when that exits 0 (runLint). Wrapping the call itself
// is the only way to see lint's own status: `lint > log; echo rc=$?` succeeds
// as a whole command either way.

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"

	"mvdan.cc/sh/v3/syntax"
)

// In the git dir, so it is per worktree and never committed.
const lintStampFile = "heph-lint-ok"

const unlintedReason = "Blocked: %s.\n" +
	"A push that `lint` would have failed costs a full CI round-trip to find out. Run `lint` (branch on its exit code: " +
	"rustfmt prints `Diff in`, not `error`), commit, then push. A `lint` run through the Bash tool records the tree it passed on.\n" +
	"If this push should go out unlinted (a WIP branch nobody reviews yet), re-run as `HEPH_PUSH_UNLINTED=1 git push ...`."

// lintWrapper is the command that replaces a `lint` word: this program, in
// lint mode, told the directory the shell is in when it gets there.
func lintWrapper(gateDir string) string {
	return "env GOTOOLCHAIN=local GOWORK=off go run -C " + shQuote(gateDir) + ` . lint "$PWD"`
}

func shQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// stampLint returns cmd with each `lint` the shell would run replaced by
// wrapper, or "" when it runs none. Only that word is spliced — the rest of
// the command is left byte for byte as written. A `lint` inside `bash -c '…'`
// is not reached: that run is simply not stamped.
func stampLint(cmd, wrapper string) string {
	file, err := parse(cmd)
	if err != nil {
		return ""
	}
	var words []*syntax.Word
	syntax.Walk(file, func(node syntax.Node) bool {
		if call, ok := node.(*syntax.CallExpr); ok {
			args := literals(call)
			rest := unwrap(args, map[string]bool{})
			if len(rest) > 0 && rest[0] == "lint" {
				words = append(words, call.Args[len(args)-len(rest)])
			}
		}
		return true
	})
	if len(words) == 0 {
		return ""
	}
	slices.SortFunc(words, func(a, b *syntax.Word) int { return int(a.Pos().Offset()) - int(b.Pos().Offset()) })
	var b strings.Builder
	last := uint(0)
	for _, w := range words {
		b.WriteString(cmd[last:w.Pos().Offset()])
		b.WriteString(wrapper)
		last = w.End().Offset()
	}
	b.WriteString(cmd[last:])
	return b.String()
}

// runLint runs `lint args...` in dir and stamps the tree it checked when it
// passes. The tree is taken before lint starts, so an edit made mid-run is not
// stamped as checked. It returns lint's exit code.
func runLint(dir string, args []string) int {
	gitDir, tree, err := workTree(dir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "gate: this lint run will not be recorded for push: %v\n", err)
	}
	cmd := exec.Command("lint", args...)
	cmd.Dir = dir
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	code := 0
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			fmt.Fprintf(os.Stderr, "gate: running lint: %v\n", err)
			return 127
		}
		if code = exit.ExitCode(); code <= 0 {
			code = 1 // killed by a signal
		}
	}
	if code == 0 && tree != "" {
		if err := writeStamp(gitDir, tree); err != nil {
			fmt.Fprintf(os.Stderr, "gate: lint passed but recording it failed: %v\n", err)
		}
	}
	return code
}

// workTree is the tree of dir's working copy as `git add -A` would stage it,
// uncommitted and untracked files included, built in a scratch index so the
// real one is never touched.
func workTree(dir string) (gitDir, tree string, err error) {
	if gitDir, err = git(dir, nil, "rev-parse", "--absolute-git-dir"); err != nil {
		return "", "", err
	}
	scratch, err := os.MkdirTemp("", "heph-lint-index-")
	if err != nil {
		return "", "", err
	}
	defer os.RemoveAll(scratch)
	index := filepath.Join(scratch, "index")
	// Seeded from the real index so `add -A` reuses its stat cache rather
	// than hashing the whole checkout. Without one git starts empty.
	if b, err := os.ReadFile(filepath.Join(gitDir, "index")); err == nil {
		if err := os.WriteFile(index, b, 0o600); err != nil {
			return "", "", err
		}
	}
	env := append(os.Environ(), "GIT_INDEX_FILE="+index)
	if _, err := git(dir, env, "add", "-A"); err != nil {
		return "", "", err
	}
	tree, err = git(dir, env, "write-tree")
	return gitDir, tree, err
}

func writeStamp(gitDir, tree string) error {
	tmp := filepath.Join(gitDir, lintStampFile+".tmp")
	if err := os.WriteFile(tmp, []byte(tree+"\n"), 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, filepath.Join(gitDir, lintStampFile))
}

// lintStamp says why HEAD in dir should not be pushed yet, or "" when the last
// passing `lint` there checked exactly HEAD's tree. Anything git cannot answer
// — not a repository, no commits — allows.
func lintStamp(dir string) string {
	if dir == "" {
		dir = "."
	}
	gitDir, err := git(dir, nil, "rev-parse", "--absolute-git-dir")
	if err != nil {
		return ""
	}
	head, err := git(dir, nil, "rev-parse", "HEAD^{tree}")
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

// git runs git in dir with env (nil: inherit) and returns its trimmed stdout.
func git(dir string, env []string, args ...string) (string, error) {
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	cmd.Env = env
	out, err := cmd.Output()
	if err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) && len(exit.Stderr) > 0 {
			return "", fmt.Errorf("git %s: %s", args[0], strings.TrimSpace(string(exit.Stderr)))
		}
		return "", fmt.Errorf("git %s: %w", args[0], err)
	}
	return strings.TrimSpace(string(out)), nil
}
