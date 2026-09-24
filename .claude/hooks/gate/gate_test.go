package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

// Both directions matter. A gate that misses `cd x && e2e` is a release build
// nobody asked for; one that fires on `cargo test -p e2e` or a commit message
// mentioning tst blocks ordinary work, and the agent learns to route around it.

func TestBlocks(t *testing.T) {
	for _, cmd := range []string{
		"tst",
		"e2e",
		"cd crates && e2e --test tui_pty",
		"timeout 600 tst 2>&1 | tail -5",
		"time tst",
		"devenv shell -- e2e",
		"devenv shell e2e",
		"env FOO=1 tst",
		"FOO=1 e2e",
		"HEPH_FULL_SUITE=0 tst",
		"echo $(tst)",
		"echo `e2e`",
		"(cd a; e2e)",
		"if true; then tst; fi",
		"for i in 1; do e2e; done",
		"bash -c 'cd x && tst'",
		"sh -ec \"e2e\"",
		"cat <<EOF > f\nx\nEOF\ntst",
		"grep x <<< 'here string' && e2e",
		"sleep 45; gh pr checks 1",
		"sleep 60s",
		"sleep 1m",
		"sleep 20 10",
		"sleep 0.5m",
		// Backticks in double quotes are a command substitution: this
		// really runs tst, which is the shell's reading and so the gate's.
		"git commit -m \"the `tst` suite\"",
	} {
		if check(cmd, gate{}) == "" {
			t.Errorf("should block %q", cmd)
		}
	}
}

func TestAllows(t *testing.T) {
	for _, cmd := range []string{
		"HEPH_FULL_SUITE=1 tst",
		"HEPH_FULL_SUITE=1 e2e --test tui_pty",
		"env HEPH_FULL_SUITE=1 tst",
		"cd x && HEPH_FULL_SUITE=1 timeout 900 e2e",
		"cargo test -p e2e some_test",
		"cargo test --test tst",
		"ls crates/bin-e2e",
		"echo tst",
		"git commit -m 'make tst faster; e2e too'",
		"python3 - <<'EOF'\ntst\n`e2e` runs on every push\nsleep 99\nEOF\necho done",
		"cat > f <<-EOF\n\te2e\n\tEOF",
		"until [ -f x ]; do sleep 5; done",
		"sleep 10",
		"sleep 0.4m",
		"sleep $DELAY",
		"lint",
		// zsh-only syntax the bash parser rejects: fail open.
		"print -r -- ${(j:,:)arr}",
	} {
		if r := check(cmd, gate{}); r != "" {
			t.Errorf("should allow %q, got: %s", cmd, r)
		}
	}
}

func TestBackgroundSleepIsNotPolling(t *testing.T) {
	if r := check("sleep 300 && echo done", gate{background: true}); r != "" {
		t.Errorf("background sleep blocked: %s", r)
	}
	if check("tst", gate{background: true}) == "" {
		t.Error("a background tst is still the full suite")
	}
}

func TestPushNeedsLint(t *testing.T) {
	stale := gate{unlinted: func() string { return "stale" }}
	for _, cmd := range []string{
		"git push",
		"git push -u origin HEAD",
		"git push --force-with-lease",
		"git -C . push",
		"cd x && git push",
		"gh stack submit --auto",
		"HEPH_PUSH_UNLINTED=0 git push",
	} {
		if check(cmd, stale) == "" {
			t.Errorf("should block %q on an unlinted tree", cmd)
		}
	}
	for _, cmd := range []string{
		"HEPH_PUSH_UNLINTED=1 git push",
		"env HEPH_PUSH_UNLINTED=1 git push -u origin HEAD",
		"git push origin --delete old-branch",
		"git push --tags",
		"git commit -m 'git push later'",
		"git status",
		"gh stack sync",
		"gh pr create --fill",
	} {
		if r := check(cmd, stale); r != "" {
			t.Errorf("should allow %q, got: %s", cmd, r)
		}
	}
	linted := gate{unlinted: func() string { return "" }}
	if r := check("git push", linted); r != "" {
		t.Errorf("push of a linted tree blocked: %s", r)
	}
}

func TestLintStamp(t *testing.T) {
	dir := t.TempDir()
	run := func(args ...string) string {
		t.Helper()
		out, err := git(dir, nil, args...)
		if err != nil {
			t.Fatalf("git %v: %v", args, err)
		}
		return out
	}
	if r := lintStamp(dir); r != "" {
		t.Errorf("not a repository yet, should allow: %s", r)
	}
	commit := func(args ...string) {
		t.Helper()
		run(append([]string{"-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q"}, args...)...)
	}
	run("init", "-q")
	commit("--allow-empty", "-m", "a")
	if lintStamp(dir) == "" {
		t.Error("no stamp should block")
	}
	gitDir := run("rev-parse", "--absolute-git-dir")
	stamp := filepath.Join(gitDir, lintStampFile)
	if err := os.WriteFile(stamp, []byte(run("rev-parse", "HEAD^{tree}")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if r := lintStamp(dir); r != "" {
		t.Errorf("stamp matches HEAD, should allow: %s", r)
	}
	if err := os.WriteFile(filepath.Join(dir, "f"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	run("add", "f")
	commit("-m", "b")
	if lintStamp(dir) == "" {
		t.Error("a commit after lint should block")
	}
}

func TestStampLintRewritesOnlyLint(t *testing.T) {
	const w = "WRAP"
	for cmd, want := range map[string]string{
		"lint":                                 "WRAP",
		"lint > /tmp/l.log 2>&1; echo rc=$?":   "WRAP > /tmp/l.log 2>&1; echo rc=$?",
		"cd x && timeout 900 lint && git push": "cd x && timeout 900 WRAP && git push",
		"devenv shell -- lint":                 "devenv shell -- WRAP",
		"lint;  lint":                          "WRAP;  WRAP",
		"echo \"$(lint)\"  # keep  spacing":    "echo \"$(WRAP)\"  # keep  spacing",
		"cat <<EOF\nlint\nEOF\nlint":           "cat <<EOF\nlint\nEOF\nWRAP",
		"echo lint":                            "",
		"git commit -m 'lint'":                 "",
		"cargo clippy -p lint":                 "",
		"linter":                               "",
		"bash -c lint":                         "",
	} {
		if got := stampLint(cmd, w); got != want {
			t.Errorf("stampLint(%q) = %q, want %q", cmd, got, want)
		}
	}
}

func TestShQuote(t *testing.T) {
	const dir = "/it's a $dir"
	out, err := exec.Command("sh", "-c", "printf %s "+shQuote(dir)).Output()
	if err != nil || string(out) != dir {
		t.Errorf("sh read %q back as %q (%v)", dir, out, err)
	}
}

// runLint is what makes a push allowed: it must stamp only a passing run, and
// stamp the tree it checked — which is HEAD's once that tree is committed.
func TestRunLintStampsOnlyAPass(t *testing.T) {
	bin := t.TempDir()
	if err := os.WriteFile(filepath.Join(bin, "lint"), []byte("#!/bin/sh\nexit $FAKE_LINT_RC\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))

	dir := t.TempDir()
	run := func(args ...string) {
		t.Helper()
		if _, err := git(dir, nil, args...); err != nil {
			t.Fatal(err)
		}
	}
	commit := func(msg string) {
		run("add", "-A")
		run("-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "-m", msg)
	}
	run("init", "-q")
	if err := os.WriteFile(filepath.Join(dir, "a"), []byte("1"), 0o644); err != nil {
		t.Fatal(err)
	}
	commit("a")

	t.Setenv("FAKE_LINT_RC", "3")
	if code := runLint(dir, nil); code != 3 {
		t.Errorf("exit code %d, want lint's 3", code)
	}
	if lintStamp(dir) == "" {
		t.Error("a failing lint must not stamp")
	}

	// Lint an uncommitted edit and a new file, then commit exactly that.
	if err := os.WriteFile(filepath.Join(dir, "a"), []byte("2"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "b"), []byte("new"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("FAKE_LINT_RC", "0")
	if code := runLint(dir, nil); code != 0 {
		t.Fatalf("exit code %d", code)
	}
	// Trimmed " M a": unstaged edit, untracked new file — as before the run.
	if status, _ := git(dir, nil, "status", "--porcelain"); status != "M a\n?? b" {
		t.Errorf("lint touched the real index: %q", status)
	}
	commit("b")
	if r := lintStamp(dir); r != "" {
		t.Errorf("committing the linted tree should allow the push: %s", r)
	}
}
