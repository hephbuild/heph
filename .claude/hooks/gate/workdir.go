package main

// Which checkout a command operates in. The hook is told the session's cwd,
// but a command can run elsewhere — `cd <worktree> && gh stack submit`,
// `git -C <worktree> push` — and the lint stamp that matters is the one in the
// checkout being pushed, not the session's. Reading the session's refused a
// push from a worktree `lint` had just passed in, and would have let an
// unlinted worktree through on the session checkout's stamp.
//
// The command is followed the way the shell runs it: `cd` (and zsh's `chdir`)
// moves the steps after it, a `{ …; }` block or an `if` passes its directory
// on, and a subshell, a command substitution or a background job keeps its
// own. Each step starts from wherever the step before it ended, failed or
// not, with one assumption: a `cd` whose failure nothing branches on is taken
// to succeed (`cd x; git push` runs in x). Where the command does branch on
// it (`&&`, `||`, `if`, `!`), both outcomes are followed. A command line run
// by another — `bash -c '…'`, `eval '…'` — is followed from where that command
// runs, with its environment: an opt-in given to it holds inside, and an
// `env -C` or a repository variable given to it leaves its directory unknown.
//
// The directory is unknown where the outcomes disagree, and wherever the gate
// cannot follow the shell:
//   - a target it cannot read: an expansion, `~`, `cd -`, a flag, a bare `cd`;
//   - `pushd`/`popd`, `eval`, `source`/`.`, `env -C` (in any option
//     cluster, `-iC`), `env -S`, whose string can hold a `-C`;
//   - `GIT_DIR` or `GIT_WORK_TREE`, assigned, exported or given to the push,
//     and a `source` or `eval`: each holds for every push after it, and no
//     later `cd` or `-C` makes the checkout known again (one in a subshell
//     stays there);
//   - a `cd` inside a loop, a `case` or a function: everything after it;
//   - the last command of a pipe that moves, which zsh — the session's
//     shell — runs in the current shell, and bash in a subshell;
//   - a function body, which runs wherever it is called;
//   - everything after a function named after a command the gate reads —
//     `git`, `gh`, `cd`, `chdir`, `pushd`, `popd`, `builtin`, `command`,
//     `eval`, `source`, `.`, a shell, or a wrapper (`env`, `nice`,
//     `timeout`, …) — since that name now runs the function.
//
// A push from an unknown directory is refused rather than read from the
// session's cwd: failing open there is the hole this file closes. So is a
// command the parser rejects, when its text mentions a push (gate.go), and a
// push no path reaches, after an `exit` or `return` (its refusal says so).
//
// The gate follows what it parses. A push it cannot see is not checked at
// all: one built by an expansion (`$CMD`, `bash -c "$S"`, `eval "$S"` without
// the word `push` in it), or run by a script, an alias or a function defined
// elsewhere. Nor are these, which it does not read:
//   - a name behind a backslash (`\git push`, `\cd /wt`): neither the push
//     nor the move is seen;
//   - a `trap` handler;
//   - a script fed to a shell on its stdin (`bash <<EOF`, `… | sh`);
//   - `xargs`, and the command in `env -S`'s string;
//   - `chdir` inside `bash -c`, read as a move although bash has no
//     `chdir`, so `bash -c 'chdir /wt; git push'` is read as a push from /wt.

import (
	"fmt"
	"maps"
	"path/filepath"
	"strings"

	"mvdan.cc/sh/v3/syntax"
)

// wd is the directory a step runs in, as far as the gate can tell.
type wd struct {
	dir   string
	state wdState
}

type wdState uint8

const (
	known wdState = iota
	// unknownDir: the gate cannot say which directory this is.
	unknownDir
	// dead: no step runs here — the shell exited before it.
	dead
)

func at(dir string) wd { return wd{dir: dir} }

var (
	unknownWD = wd{state: unknownDir}
	deadWD    = wd{state: dead}
)

// merge is the directory after one of two paths, not knowing which ran.
func merge(a, b wd) wd {
	switch {
	case a.state == dead:
		return b
	case b.state == dead, a == b:
		return a
	}
	return unknownWD
}

// cd is d moved by `cd target` (or git's `-C target`). target is "" when it
// depends on expansion; it cannot be read either when it is a flag, `-`, or
// starts with `~`.
func (d wd) cd(target string) wd {
	switch {
	case d.state == dead:
		return d
	case target == "", strings.HasPrefix(target, "-"), strings.HasPrefix(target, "~"):
		return unknownWD
	case filepath.IsAbs(target):
		return at(filepath.Clean(target))
	case d.state == unknownDir:
		return d
	}
	return at(filepath.Join(d.dir, target))
}

// out is where a step leaves the shell, by how it ended.
type out struct {
	ok, fail wd
	// cdFail is where a failure that can only be a `cd`'s own leaves it: the
	// directory the cd did not move from. The next statement does not start
	// there — `cd /wt; git push` is read as a push from /wt — but `||`, `if`
	// and `!`, which branch on the failure, follow it as any other.
	cdFail wd
}

func same(d wd) out { return out{ok: d, fail: d, cdFail: deadWD} }

var deadOut = out{ok: deadWD, fail: deadWD, cdFail: deadWD}

// failed is the directory after any failure, a failed cd's included.
func (o out) failed() wd { return merge(o.fail, o.cdFail) }

// next is where the statement after o starts: wherever o ended, except after
// a failed cd.
func (o out) next() wd { return merge(o.ok, o.fail) }

// either is the outcome of a or b, not knowing which ran.
func either(a, b out) out {
	return out{ok: merge(a.ok, b.ok), fail: merge(a.fail, b.fail), cdFail: merge(a.cdFail, b.cdFail)}
}

// walker checks every command of a command line in the directory it runs in,
// and keeps the first refusal.
type walker struct {
	g      gate
	reason string
}

// list runs stmts from in and returns where the last one leaves the shell.
// Each statement after the first starts from the one before it, whether that
// succeeded or failed.
func (w *walker) list(stmts []*syntax.Stmt, in wd) out {
	o := same(in)
	for i, s := range stmts {
		if i > 0 {
			in = o.next()
		}
		o = w.stmt(s, in)
	}
	return o
}

func (w *walker) stmt(s *syntax.Stmt, in wd) out {
	if w.reason != "" {
		return same(in)
	}
	w.substs(in, s.Redirs)
	if s.Background || s.Coprocess || s.Disown {
		// A background job runs in a subshell: it moves nothing after it.
		w.subshell(func() { w.cmd(s, in) })
		return same(in)
	}
	o := w.cmd(s, in)
	if s.Negated {
		// `!` branches on the outcome: a failed cd is as real as any other.
		return out{ok: o.failed(), fail: o.ok, cdFail: deadWD}
	}
	return o
}

func (w *walker) cmd(s *syntax.Stmt, in wd) out {
	switch c := s.Cmd.(type) {
	case nil:
		return same(in)
	case *syntax.CallExpr:
		return w.call(c, s.Redirs, in)
	case *syntax.BinaryCmd:
		switch c.Op {
		case syntax.AndStmt:
			x := w.stmt(c.X, in)
			y := w.stmt(c.Y, x.ok)
			return out{ok: y.ok, fail: merge(x.fail, y.fail), cdFail: merge(x.cdFail, y.cdFail)}
		case syntax.OrStmt:
			x := w.stmt(c.X, in)
			y := w.stmt(c.Y, x.failed())
			return out{ok: merge(x.ok, y.ok), fail: y.fail, cdFail: y.cdFail}
		}
		// A pipe. Every command but the last runs in a subshell. The last does
		// in bash, but in zsh — the session's shell — it runs in the current
		// one: a cd there leaves the directory unknown. Pipes nest to the left,
		// so c.Y is the last command.
		w.subshell(func() { w.stmt(c.X, in) })
		w.stmt(c.Y, in)
		if movesDir(c.Y) {
			return same(unknownWD)
		}
		return same(in)
	case *syntax.Subshell:
		w.subshell(func() { w.list(c.Stmts, in) })
		return same(in)
	case *syntax.Block:
		return w.list(c.Stmts, in)
	case *syntax.IfClause:
		return w.ifClause(c, in)
	case *syntax.TimeClause:
		if c.Stmt == nil {
			return same(in)
		}
		return w.stmt(c.Stmt, in)
	case *syntax.FuncDecl:
		// The body runs wherever the function is called, which the gate does
		// not follow; and calling one that moves moves the caller.
		w.nested(c.Body, unknownWD)
		if c.Name != nil && followed[c.Name.Value] {
			// From here on that name runs the function: `git() { command git
			// -C /wt "$@"; }` turns every `git push` after it into a push
			// from /wt, and `cd() { :; }` every cd into nothing.
			w.g.checkoutUnknown = true
		}
		if movesDir(c.Body) {
			return same(unknownWD)
		}
		return same(in)
	}
	// Loops, case, `[[ ]]`, declarations: their statements are checked, but
	// a cd among them leaves every directory in and after them unknown, and
	// a repository named among them holds for all of them, as a loop can
	// come round to the first after the last.
	if movesDir(s.Cmd) {
		in = unknownWD
	}
	if setsRepo(s.Cmd) {
		w.g.checkoutUnknown = true
	}
	w.nested(s.Cmd, in)
	return same(in)
}

// subshell walks f as a subshell: a repository it names, or a function it
// defines, is its own, and does not reach the commands after it.
func (w *walker) subshell(f func()) {
	saved := w.g.checkoutUnknown
	f()
	w.g.checkoutUnknown = saved
}

func (w *walker) ifClause(c *syntax.IfClause, in wd) out {
	if len(c.Cond) == 0 { // else
		return w.list(c.Then, in)
	}
	cond := w.list(c.Cond, in)
	then := w.list(c.Then, cond.ok)
	// No branch taken: the if succeeds where its condition failed.
	els := out{ok: cond.failed(), fail: deadWD, cdFail: deadWD}
	if c.Else != nil {
		els = w.ifClause(c.Else, cond.failed())
	}
	return either(then, els)
}

func (w *walker) call(c *syntax.CallExpr, redirs []*syntax.Redirect, in wd) out {
	w.substs(in, c.Assigns, c.Args)
	if len(c.Args) == 0 {
		// A bare assignment: one naming git's repository holds for every
		// command after it, wherever it runs.
		if assignsRepo(c.Assigns) {
			w.g.checkoutUnknown = true
		}
		return same(in)
	}
	if args := builtinArgs(literals(c)); len(args) > 0 {
		switch args[0] {
		case "cd", "chdir":
			if len(args) != 2 {
				return out{ok: unknownWD, fail: deadWD, cdFail: in}
			}
			return out{ok: in.cd(args[1]), fail: deadWD, cdFail: in}
		case "pushd", "popd":
			return out{ok: unknownWD, fail: deadWD, cdFail: in}
		case "eval":
			w.eval(c, c.Args[len(c.Args)-len(args)+1:], in)
			w.g.checkoutUnknown = true
			return same(unknownWD)
		case "source", ".":
			// A script run in this shell can leave it anywhere, and point git
			// anywhere.
			w.g.checkoutUnknown = true
			return same(unknownWD)
		case "exit", "return":
			return deadOut
		}
	}
	if w.reason == "" {
		w.reason = checkCall(c, redirs, in, w.g)
	}
	return same(in)
}

// eval checks the command line `eval` runs, words joined, started from in
// with eval's environment. When a word depends on expansion the line cannot
// be read, and one that mentions a push is refused.
func (w *walker) eval(c *syntax.CallExpr, words []*syntax.Word, in wd) {
	if w.reason != "" {
		return
	}
	env := commandEnv(w.g, c.Assigns)
	parts := make([]string, 0, len(words))
	for _, word := range words {
		lit := literal(word)
		if lit == "" {
			var b strings.Builder
			if err := syntax.NewPrinter().Print(&b, c); err == nil && w.g.unlinted != nil &&
				mentionsPush(b.String()) && env[unlintedOptIn] != "1" {
				w.reason = fmt.Sprintf(unreadablePushReason, "what this `eval` runs, which depends on an expansion")
			}
			return
		}
		parts = append(parts, lit)
	}
	g, start := w.g.nested(env, false, in)
	w.reason = checkIn(strings.Join(parts, " "), start, g)
}

// substs checks the command substitutions in nodes, each a subshell started
// from in.
func (w *walker) substs(in wd, nodes ...any) {
	for _, n := range nodes {
		switch n := n.(type) {
		case []*syntax.Redirect:
			for _, r := range n {
				w.nested(r, in)
			}
		case []*syntax.Assign:
			for _, a := range n {
				w.nested(a, in)
			}
		case []*syntax.Word:
			for _, word := range n {
				w.nested(word, in)
			}
		}
	}
}

// nested checks every statement under node, from in.
func (w *walker) nested(node syntax.Node, in wd) {
	syntax.Walk(node, func(n syntax.Node) bool {
		switch n := n.(type) {
		case *syntax.Stmt:
			w.stmt(n, in)
			return false
		case *syntax.CmdSubst:
			w.subshell(func() { w.list(n.Stmts, in) })
			return false
		case *syntax.ProcSubst:
			w.subshell(func() { w.list(n.Stmts, in) })
			return false
		}
		return true
	})
}

// builtinArgs is a command line as the shell's builtins see it: `builtin cd x`
// and `command cd x` are `cd x`. Only these two keep running in the shell —
// `env cd x` runs an external cd that moves nothing.
func builtinArgs(args []string) []string {
	if len(args) > 1 && (args[0] == "builtin" || args[0] == "command") {
		return args[1:]
	}
	return args
}

// followed are the commands the gate reads to work out which checkout a push
// runs in: git and gh, what moves the shell, what runs a command line or
// another command (wrappers). A function named after one of them changes
// what that name does, so the gate can no longer say.
var followed = func() map[string]bool {
	m := map[string]bool{
		"git": true, "gh": true, "builtin": true, "command": true,
		"bash": true, "sh": true, "zsh": true, "devenv": true,
	}
	maps.Copy(m, shellMoves)
	for name := range wrappers {
		m[name] = true
	}
	return m
}()

// shellMoves are the commands that can leave the shell they run in in
// another directory.
var shellMoves = map[string]bool{
	"cd": true, "chdir": true, "pushd": true, "popd": true,
	"eval": true, "source": true, ".": true,
}

// movesDir reports whether a command under node can leave the shell in
// another directory. A subshell — `( )`, `$( )`, `<( )` — moves only itself,
// and is not looked into.
func movesDir(node syntax.Node) bool {
	return inShell(node, func(n syntax.Node) bool {
		c, ok := n.(*syntax.CallExpr)
		if !ok || len(c.Args) == 0 {
			return false
		}
		args := builtinArgs(literals(c))
		return len(args) > 0 && shellMoves[args[0]]
	})
}

// setsRepo reports whether a command under node can point git at another
// repository for the commands after it: GIT_DIR or GIT_WORK_TREE assigned or
// declared, or a `source` or `eval`, which can do either. A subshell is not
// looked into.
func setsRepo(node syntax.Node) bool {
	return inShell(node, func(n syntax.Node) bool {
		switch n := n.(type) {
		case *syntax.CallExpr:
			if len(n.Args) == 0 {
				return assignsRepo(n.Assigns)
			}
			args := builtinArgs(literals(n))
			return len(args) > 0 && (args[0] == "eval" || args[0] == "source" || args[0] == ".")
		case *syntax.DeclClause:
			return declaresRepo(n.Args)
		}
		return false
	})
}

// inShell reports whether match holds for a node under node that runs in the
// shell itself, outside any subshell.
func inShell(node syntax.Node, match func(syntax.Node) bool) bool {
	found := false
	syntax.Walk(node, func(n syntax.Node) bool {
		switch n.(type) {
		case *syntax.Subshell, *syntax.CmdSubst, *syntax.ProcSubst:
			return false
		}
		found = found || match(n)
		return !found
	})
	return found
}

// gitRepoVars are the environment variables that point git at a repository
// other than the one its directory is in.
var gitRepoVars = map[string]bool{"GIT_DIR": true, "GIT_WORK_TREE": true}

// assignsRepo reports whether assigns set one of gitRepoVars.
func assignsRepo(assigns []*syntax.Assign) bool {
	for _, a := range assigns {
		if a.Name != nil && gitRepoVars[a.Name.Value] {
			return true
		}
	}
	return false
}

// declaresRepo reports whether a declaration's arguments (`export`,
// `declare`, `local`, …) can set one of gitRepoVars: by name, by a word that
// spells the assignment in quotes (`export "GIT_DIR=x"`, which the parser
// leaves a plain word), or by a word whose name cannot be read at all
// (`export "$V"`).
func declaresRepo(args []*syntax.Assign) bool {
	for _, a := range args {
		if a.Name != nil {
			if gitRepoVars[a.Name.Value] {
				return true
			}
			continue
		}
		word := literal(a.Value)
		switch {
		case word == "":
			return true
		case strings.HasPrefix(word, "-"), strings.HasPrefix(word, "+"):
			// An option: `declare -x`, `export -n`.
			continue
		}
		if name, _, _ := strings.Cut(word, "="); gitRepoVars[name] {
			return true
		}
	}
	return false
}

// namesRepo reports whether env sets one of gitRepoVars.
func namesRepo(env map[string]string) bool {
	for name := range gitRepoVars {
		if _, ok := env[name]; ok {
			return true
		}
	}
	return false
}

// mentionsPush reports whether text could hold a command that pushes, when
// the gate cannot read it as one.
func mentionsPush(text string) bool {
	return strings.Contains(text, "push") || (strings.Contains(text, "stack") && strings.Contains(text, "submit"))
}

// gitValueFlags are git's global options whose value is the next argument
// (`--git-dir=<path>` is one argument, and needs no entry).
var gitValueFlags = map[string]bool{
	"-C": true, "-c": true, "--config-env": true,
	"--git-dir": true, "--work-tree": true, "--namespace": true,
}

// gitCmd is a git command line read past its global options.
type gitCmd struct {
	// sub is the index of the subcommand, len(args) when there is none.
	sub int
	// chdir is each `-C <path>`, in order.
	chdir []string
	// repo is set when `--git-dir` or `--work-tree` points git at a
	// repository the -C chain does not name.
	repo bool
}

// parseGit walks git's global options in args, a `git ...` command line.
func parseGit(args []string) gitCmd {
	var g gitCmd
	i := 1
	for ; i < len(args) && strings.HasPrefix(args[i], "-"); i++ {
		flag, _, inline := strings.Cut(args[i], "=")
		if flag == "--git-dir" || flag == "--work-tree" {
			g.repo = true
		}
		if inline || !gitValueFlags[flag] {
			continue
		}
		i++
		if flag == "-C" && i < len(args) {
			g.chdir = append(g.chdir, args[i])
		}
	}
	g.sub = min(i, len(args))
	return g
}

// pushDir is the checkout a git or gh command line started in dir operates
// on: dir, moved by each of git's `-C <path>` in turn.
func pushDir(args []string, dir wd) wd {
	if args[0] != "git" {
		return dir
	}
	g := parseGit(args)
	if g.repo {
		return unknownWD
	}
	for _, c := range g.chdir {
		dir = dir.cd(c)
	}
	return dir
}
