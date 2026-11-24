package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// The push check reads the stamp of the checkout the command pushes from. On
// 2026-11-23 `lint` passed in a worktree, stamped that worktree, and the push
// from it (`cd <worktree> && gh stack submit`) was refused on the session
// checkout's stamp.

// unknown is a push from a directory the gate cannot work out: it is refused
// without reading any stamp.
const unknown = "?"

// never is a push no path reaches (it follows an `exit`): it is refused with
// that reason, not as a push from an unknown directory.
const never = "never"

func TestPushChecksTheCheckoutItRunsIn(t *testing.T) {
	const cwd = "/session"
	for cmd, want := range map[string]string{
		"git push":                                   "/session",
		"gh stack submit --auto":                     "/session",
		"cd /wt && git push":                         "/wt",
		"cd /wt; git push":                           "/wt",
		"cd /wt\ngit push":                           "/wt",
		"cd /wt && gh stack submit --auto":           "/wt",
		"cd 'a dir' && git push":                     "/session/a dir",
		"cd wt && git push":                          "/session/wt",
		"cd /wt && cd sub && git push":               "/wt/sub",
		"cd /wt/sub && cd .. && git push":            "/wt",
		"echo hi && cd /wt && git push":              "/wt",
		"cd /wt && HEPH_PUSH_UNLINTED=0 git push":    "/wt",
		"cd /wt && timeout 60 git push":              "/wt",
		"git -C /wt push origin b":                   "/wt",
		"git -C wt push":                             "/session/wt",
		"git -C /wt -C sub push":                     "/wt/sub",
		"git -c k=v -C /wt push":                     "/wt",
		"timeout 60 git -C /wt push":                 "/wt",
		"cd /a && git -C b push":                     "/a/b",
		"cd /a && git -C /wt push":                   "/wt",
		"bash -c 'cd /wt && git push'":               "/wt",
		"cd /wt && bash -c 'git push'":               "/wt",
		"cd /wt && echo \"$(git push)\"":             "/wt",
		"lint > /tmp/l.log; cd /wt && git push":      "/wt",
		"cd /wt && lint && git -C /wt push origin b": "/wt",
		"cd \"$X\" && cd /wt && git push":            "/wt",
		// git's global options, the ones that take a value included.
		"git --namespace ns -C /wt push":   "/wt",
		"git --namespace=ns push":          "/session",
		"git --no-pager -C /wt push":       "/wt",
		"git --config-env k=V -C /wt push": "/wt",
		"git --git-dir /wt/.git push":      unknown,
		"git --git-dir=/wt/.git push":      unknown,
		"git -C /wt --work-tree /wt2 push": unknown,
		"git --work-tree=/wt2 push":        unknown,
		// Compound commands: their bodies run from the directory at entry.
		"cd /wt && git push || true":                   "/wt",
		"(cd /wt && git push)":                         "/wt",
		"{ cd /wt; git push; }":                        "/wt",
		"{ cd /wt; }; git push":                        "/wt",
		"if cd /wt; then git push; fi":                 "/wt",
		"if cd /wt; then :; else exit 1; fi; git push": "/wt",
		"echo \"$(cd /wt && git push)\"":               "/wt",
		// A cd that fails ends the command, so the steps after it run in its
		// directory.
		"cd /wt || exit 1; git push":                 "/wt",
		"cd /wt || { echo no; exit 1; }; git push":   "/wt",
		"cd /wt || { echo no; exit 1; } && git push": "/wt",
		// Nothing the gate can read with confidence: refused, not read from the
		// session's cwd.
		"cd \"$WT\" && git push":                unknown,
		"cd ~/wt && git push":                   unknown,
		"cd - && git push":                      unknown,
		"cd && git push":                        unknown,
		"cd -P /wt && git push":                 unknown,
		"cd /a && cd \"$X\" && git push":        unknown,
		"cd \"$X\" && cd wt && git push":        unknown,
		"git -C \"$WT\" push":                   unknown,
		"cd /a && git -C \"$WT\" push":          unknown,
		"cd /wt && git -C \"$X\" push":          unknown,
		"pushd /wt && git push":                 unknown,
		"pushd /wt; popd; git push":             unknown,
		"cd /wt || echo no; git push":           unknown,
		"if cd /wt; then :; fi; git push":       unknown,
		"for d in a; do cd /wt; git push; done": unknown,
		"for d in a; do cd /wt; done; git push": unknown,
		// The next statement starts from the last one's success and its
		// failure. Only a failure that can only be the cd's own is set aside
		// (`cd /wt; git push`): one from the step before a `cd /wt &&` reaches
		// the push in the session checkout.
		"[ \"$CI\" ] && cd /wt; git push":              unknown,
		"git fetch && cd /wt; git push":                unknown,
		"cd /wt && lint; git push":                     "/wt",
		"cd /wt && lint && cd sub; git push":           unknown,
		"{ echo; cd /wt; }; git push":                  "/wt",
		"cd /wt || cd /b; git push":                    unknown,
		"! cd /wt; git push":                           unknown,
		"if ! cd /wt; then git push; fi":               "/session",
		"if ! cd /wt; then exit 1; fi; git push":       "/wt",
		"cd /wt && false || git push":                  unknown,
		"cd /wt && { false || git push; }":             "/wt",
		"true | cd /wt; git push":                      unknown,
		"true |& cd /wt; git push":                     unknown,
		"cd /wt | cd /b | cat; git push":               "/session",
		"chdir /wt && git push":                        "/wt",
		"builtin chdir /wt; git push":                  "/wt",
		"chdir \"$X\" && git push":                     unknown,
		"f() { git push; }; cd /wt && f":               unknown,
		"f() { cd /wt && git push; }":                  "/wt",
		"GIT_DIR=/wt/.git git push":                    unknown,
		"GIT_WORK_TREE=/wt git push":                   unknown,
		"cd /wt && GIT_DIR=/b/.git git push":           unknown,
		"env GIT_DIR=/wt/.git git push":                unknown,
		"GIT_DIR=/wt/.git; git push":                   unknown,
		"export GIT_DIR=/wt/.git; git push":            unknown,
		"env -C /wt git push":                          unknown,
		"env -C/wt git push":                           unknown,
		"env -u X git -C /wt push":                     "/wt",
		"timeout -s KILL 60 git -C /wt push":           "/wt",
		"nice -n 5 git -C /wt push":                    "/wt",
		"env --chdir=/wt git push":                     unknown,
		"eval \"cd /wt\"; git push":                    unknown,
		"eval \"$X\"; git push":                        unknown,
		"cd /wt && eval 'git push'":                    "/wt",
		"source ./x.sh; git push":                      unknown,
		". ./x.sh && git push":                         unknown,
		"for d in a; do true | cd /wt; done; git push": unknown,
		// Pointing git at another repository holds for the rest of the command
		// line: no later cd or -C names the checkout it pushes.
		"export GIT_DIR=/wt/.git; cd /b; git push":                 unknown,
		"export GIT_DIR=/wt/.git; git -C /b push":                  unknown,
		"GIT_DIR=/wt/.git; cd /b && git push":                      unknown,
		"export \"GIT_DIR=/wt/.git\"; git push":                    unknown,
		"export 'GIT_WORK_TREE=/wt'; cd /b && git push":            unknown,
		"declare -x GIT_WORK_TREE=/wt; git -C /b push":             unknown,
		"export \"$V\"; cd /b && git push":                         unknown,
		"source ./x.sh; cd /b && git push":                         unknown,
		"eval \"$X\"; git -C /b push":                              unknown,
		"for d in a; do export GIT_DIR=x; done; cd /b && git push": unknown,
		"export X=1; cd /b && git push":                            "/b",
		"export -n X; git -C /b push":                              "/b",
		"(export GIT_DIR=/wt/.git); cd /b && git push":             "/b",
		"echo \"$(GIT_DIR=/x; :)\"; cd /b && git push":             "/b",
		// A command line run by another sees the environment it was given.
		"env -C /wt bash -c 'git push'":                  unknown,
		"env --chdir=/wt sh -c 'git push'":               unknown,
		"env -C /wt bash -c 'cd /b && git push'":         "/b",
		"GIT_DIR=/wt/.git bash -c 'git push'":            unknown,
		"env GIT_DIR=/wt/.git bash -c 'cd /b; git push'": unknown,
		"GIT_DIR=/wt/.git eval 'git push'":               unknown,
		"GIT_DIR=/wt/.git eval 'cd /b; git push'":        unknown,
		"export GIT_DIR=/x; bash -c 'cd /b; git push'":   unknown,
		"env -iC /wt git push":                           unknown,
		"env -iC/wt git push":                            unknown,
		"env -uC git push":                               unknown,
		"env -S '-C /wt' git push":                       unknown,
		"env --split-string='-C /wt' git push":           unknown,
		"env -i git -C /wt push":                         "/wt",
		"env -iu X git -C /wt push":                      "/wt",
		// A function named after a command the gate follows: what that name runs
		// from then on is the function's.
		"git() { command git -C /wt \"$@\"; }; git push": unknown,
		"cd() { :; }; cd /wt; git push":                  unknown,
		"function git { :; }; git -C /wt push":           unknown,
		"gh() { :; }; cd /wt && gh stack submit --auto":  unknown,
		"pushd() { :; }; git -C /wt push":                unknown,
		"eval() { :; }; git -C /wt push":                 unknown,
		"env() { :; }; env git -C /wt push":              unknown,
		"command() { :; }; command git -C /wt push":      unknown,
		"timeout() { :; }; cd /wt && timeout 9 git push": unknown,
		"nice() { :; }; nice git -C /wt push":            unknown,
		"f() { :; }; git -C /wt push":                    "/wt",
		"git push; git() { :; }":                         "/session",
		// A cd that does not move the steps after it.
		"(cd /wt && make); git push":                       "/session",
		"cd /wt & git push":                                "/session",
		"cd /wt | cat; git push":                           "/session",
		"bash -c 'cd /wt' && git push":                     "/session",
		"env cd /wt && git push":                           "/session",
		"export X=$(cd /a && pwd); git push":               "/session",
		"local X=\"$(cd /a && pwd)\"; git push":            "/session",
		"for d in a; do echo \"$(cd /a)\"; done; git push": "/session",
		"for d in a; do (cd /a); done; git push":           "/session",
		"for d in <(cd /a; ls); do :; done; git push":      "/session",
		"f() { (cd /wt); }; git push":                      "/session",
		// No path reaches the push.
		"exit 1; git push":               never,
		"{ exit 1; }; git push":          never,
		"cd /wt || exit; exit; git push": never,
	} {
		var got []string
		g := gate{cwd: cwd, unlinted: func(dir string, _ bool) string {
			got = append(got, dir)
			return ""
		}}
		reason := check(cmd, g)
		if want == never {
			if len(got) != 0 || !strings.Contains(reason, "after an `exit`") {
				t.Errorf("%q checked the stamp of %q (reason %q), want a refusal naming the `exit` before it", cmd, got, reason)
			}
			continue
		}
		if want == unknown {
			if len(got) != 0 || !strings.Contains(reason, "git -C <abs> push") || !strings.Contains(reason, unlintedOptIn+"=1") {
				t.Errorf("%q checked the stamp of %q (reason %q), want a refusal naming `git -C <abs> push`", cmd, got, reason)
			}
			continue
		}
		if len(got) != 1 || got[0] != want {
			t.Errorf("%q checked the stamp of %q, want [%s]", cmd, got, want)
		}
	}
}

// A command the parser rejects cannot be followed. One that mentions a push is
// refused rather than let through unread; `git push &!` is zsh's disown, which
// the bash parser does not know.
func TestUnparsablePushIsRefused(t *testing.T) {
	g := gate{cwd: "/session", unlinted: func(string, bool) string { return "" }}
	for _, cmd := range []string{
		"git push &!",
		"cd /wt && git push origin b &!",
		"gh stack submit --auto &!",
		"git push; echo 'unterminated",
	} {
		if r := check(cmd, g); !strings.Contains(r, "cannot parse") || !strings.Contains(r, unlintedOptIn+"=1") {
			t.Errorf("%q: unparsable push, want a refusal; got %q", cmd, r)
		}
	}
	for _, cmd := range []string{
		"echo 'unterminated",
		"git status &!",
		unlintedOptIn + "=1 git push &!",
	} {
		if r := check(cmd, g); r != "" {
			t.Errorf("%q: no push to check, or opted in, should allow: %s", cmd, r)
		}
	}
	if r := check("git push &!", gate{cwd: "/session"}); r != "" {
		t.Errorf("with no push check, an unparsable push allows: %s", r)
	}
}

// The reported case and its reverse, against real stamps: each checkout is
// judged on its own.
func TestPushReadsTheStampOfTheCheckoutItPushes(t *testing.T) {
	session, wt := committedRepo(t), committedRepo(t)
	stamp(t, wt)
	g := gate{cwd: session, unlinted: lintStamp}
	for _, cmd := range []string{
		"cd " + shQuote(wt) + " && gh stack submit --auto",
		"git -C " + shQuote(wt) + " push origin b",
	} {
		if r := check(cmd, g); r != "" {
			t.Errorf("%q: worktree is linted, should allow: %s", cmd, r)
		}
	}
	if check("git push", g) == "" {
		t.Error("the session checkout is not linted, its own push should block")
	}

	g = gate{cwd: wt, unlinted: lintStamp}
	if r := check("git push", g); r != "" {
		t.Errorf("worktree is linted, should allow: %s", r)
	}
	if check("cd "+shQuote(session)+" && git push", g) == "" {
		t.Error("a push from an unlinted checkout passed on the session checkout's stamp")
	}
}

// Where git cannot answer, only the session's own checkout is let through: a
// directory the command moved to is one the agent chose, and `cd /typo; git
// push` must not pass because /typo is not a repository.
func TestPushFromADirGitCannotReadIsRefused(t *testing.T) {
	session, notRepo, noCommits := committedRepo(t), t.TempDir(), t.TempDir()
	stamp(t, session)
	if _, err := git(noCommits, nil, "init", "-q"); err != nil {
		t.Fatal(err)
	}
	g := gate{cwd: session, unlinted: lintStamp}
	for _, cmd := range []string{
		"cd " + shQuote(filepath.Join(session, "typo")) + "; git push",
		"cd " + shQuote(notRepo) + " && git push",
		"git -C " + shQuote(notRepo) + " push",
		"cd " + shQuote(noCommits) + " && gh stack submit --auto",
	} {
		if r := check(cmd, g); !strings.Contains(r, "git cannot read the checkout") {
			t.Errorf("%q: git cannot answer for the checkout it pushes from, should refuse; got %q", cmd, r)
		}
	}
	if r := check("cd "+shQuote(session)+" && git push", g); r != "" {
		t.Errorf("the session's own linted checkout, reached by cd, should allow: %s", r)
	}
	if r := check("git push", gate{cwd: notRepo, unlinted: lintStamp}); r != "" {
		t.Errorf("a session cwd git cannot read allows, as before: %s", r)
	}
}

// The recording side: the rewritten `lint` stamps the checkout the shell is in
// when it runs, not the one the session started in.
func TestLintStampsTheCheckoutItRunsIn(t *testing.T) {
	if _, err := exec.LookPath("go"); err != nil {
		t.Skip("go is not on PATH")
	}
	gateDir, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	bin := t.TempDir()
	if err := os.WriteFile(filepath.Join(bin, "lint"), []byte("#!/bin/sh\nexit 0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))

	session, wt := committedRepo(t), committedRepo(t)
	cmd := exec.Command("sh", "-c", stampLint("cd "+shQuote(wt)+" && lint", lintWrapper(gateDir)))
	cmd.Dir = session
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("rewritten lint: %v\n%s", err, out)
	}
	if r := lintStamp(wt, false); r != "" {
		t.Errorf("lint ran in the worktree but did not stamp it: %s", r)
	}
	if lintStamp(session, false) == "" {
		t.Error("lint ran in the worktree but stamped the session checkout")
	}
}

// committedRepo is a new repository with one commit.
func committedRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for _, args := range [][]string{
		{"init", "-q"},
		{"-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "a"},
	} {
		if _, err := git(dir, nil, args...); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

// stamp records HEAD's tree in dir as linted.
func stamp(t *testing.T, dir string) {
	t.Helper()
	gitDir, err := git(dir, nil, "rev-parse", "--absolute-git-dir")
	if err != nil {
		t.Fatal(err)
	}
	tree, err := git(dir, nil, "rev-parse", "HEAD^{tree}")
	if err != nil {
		t.Fatal(err)
	}
	if err := writeStamp(gitDir, tree); err != nil {
		t.Fatal(err)
	}
}
