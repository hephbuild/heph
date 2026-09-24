package main

import "testing"

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
		if check(cmd, false) == "" {
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
		if r := check(cmd, false); r != "" {
			t.Errorf("should allow %q, got: %s", cmd, r)
		}
	}
}

func TestBackgroundSleepIsNotPolling(t *testing.T) {
	if r := check("sleep 300 && echo done", true); r != "" {
		t.Errorf("background sleep blocked: %s", r)
	}
	if check("tst", true) == "" {
		t.Error("a background tst is still the full suite")
	}
}
