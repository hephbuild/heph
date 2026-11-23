package main

import (
	"fmt"
	"maps"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"mvdan.cc/sh/v3/syntax"
)

// Prefix assignments, set to 1, that let a refused command through.
const (
	optIn         = "HEPH_FULL_SUITE"
	unlintedOptIn = "HEPH_PUSH_UNLINTED"
	editOptIn     = "HEPH_SCRIPTED_EDIT"
)

const editReason = "Blocked: a scripted edit (`sed -i`, `perl -i`, or a python read-replace-write). Use the Edit tool — several changes are several Edit calls in one message.\n" +
	"A script hides the diff from the user and leaves the harness's view of the file stale; chained line-number `sed -i` edits also land on the wrong lines once an earlier one inserts.\n" +
	"The one exception is a signature change whose every call site cargo lists: prefix this command with `HEPH_SCRIPTED_EDIT=1`, limit it to those file:lines, and follow it with `git diff -U0`. See CLAUDE.md \"Session economy\"."

const tstReason = "Blocked: `tst` is the full suite, and CI runs it on every push.\n" +
	"Run the tests for what you changed: `cargo test -p <crate> <name>`, then push.\n" +
	"Only a large blast-radius change (engine core, provider/driver traits, caching) runs it locally, before opening the PR — " +
	"if that is this change, re-run as `HEPH_FULL_SUITE=1 tst`. See CLAUDE.md \"Workflow\"."

const cargoTestReason = "Blocked: `cargo test` over the whole workspace is the full suite — what `tst` runs — and CI runs it on every push.\n" +
	"Run the tests for what you changed: `cargo test -p <crate> <name>`.\n" +
	"If this change does need the full suite locally (engine core, provider/driver traits, caching), prefix this same command with `HEPH_FULL_SUITE=1`. See CLAUDE.md \"Workflow\"."

const e2eReason = "Blocked: `e2e` does a full --release build, and the bin_e2e CI job runs it on all three platforms on every push.\n" +
	"Run it locally only when changing what it covers — the plugin loader, the TUI, CLI exit codes, or the e2e script itself.\n" +
	"If that is this change, re-run as `HEPH_FULL_SUITE=1 e2e ...` (and check the `running N tests` count). See CLAUDE.md \"e2e\"."

const sleepReason = "Blocked: foreground `sleep %s`. Don't poll.\n" +
	"Waiting on CI: `gh run watch <run-id> --exit-status` with run_in_background — the session is woken when it exits.\n" +
	"Waiting on a local process: run it with run_in_background, or use Monitor with an until-loop."

const unknownDirReason = "Blocked: this push runs in a directory the gate cannot work out, so it cannot check that `lint` passed there — " +
	"a `cd` or `-C` it cannot read (an expansion, `~`, `cd -`, a flag, a bare `cd`); `pushd`, `eval`, `source` or `env -C`; " +
	"`GIT_DIR`, `GIT_WORK_TREE`, `source` or `eval` anywhere before the push, which no later `cd` or `-C` undoes; a function named after a command the gate reads (`git`, `cd`, `env`, …); a `cd` inside a loop or a function, or as the last command of a pipe; a push inside a function body; " +
	"or a step whose failure, or success, reaches the push from another directory (`git fetch && cd <abs>; git push`).\n" +
	"Name the checkout as a literal absolute path: `git -C <abs> push ...`, or `cd <abs> && gh stack submit ...`. " +
	"If an earlier `GIT_DIR`, `GIT_WORK_TREE`, `source`, `eval` or redefined command is the cause, that is not enough: run the push as its own command.\n" +
	"If this push should go out unlinted, re-run as `HEPH_PUSH_UNLINTED=1 git push ...`."

const deadPushReason = "Blocked: this push comes after an `exit` or `return` on every path, so it never runs, and the gate cannot say which checkout it would push from.\n" +
	"Drop it, or move it before the `exit`.\n" +
	"If this push should go out unlinted, re-run as `HEPH_PUSH_UNLINTED=1 git push ...`."

const unreadablePushReason = "Blocked: the gate cannot read %s, and it mentions a push, so it cannot check that `lint` passed in the checkout that push runs in.\n" +
	"Write the push as a plain command with the checkout as a literal absolute path: `git -C <abs> push ...`, or `cd <abs> && gh stack submit ...`.\n" +
	"If this push should go out unlinted, re-run as `HEPH_PUSH_UNLINTED=1 git push ...`."

// Wrappers that run their arguments as a command. The value is how many
// arguments of their own they take before the command starts.
var wrappers = map[string]int{
	"command": 0, "exec": 0, "nohup": 0, "nice": 0, "env": 0, "timeout": 1,
}

// wrapperValueFlags are the wrappers' options whose value is the next
// argument. env's short options are read as a cluster (envCluster).
var wrapperValueFlags = map[string]map[string]bool{
	"env":     {"--chdir": true, "--unset": true, "--split-string": true, "--argv0": true},
	"nice":    {"-n": true, "--adjustment": true},
	"timeout": {"-s": true, "--signal": true, "-k": true, "--kill-after": true},
}

// gate is what a check needs besides the command itself.
type gate struct {
	background bool
	// cwd is the directory the command starts in: the session's.
	cwd string
	// unlinted says why HEAD of the checkout at dir should not be pushed yet,
	// or "" when `lint` passed there on exactly HEAD's tree. Called only for a
	// command that pushes, with the checkout it pushes from (workdir.go), and
	// elsewhere set when that is not cwd; nil allows every push.
	unlinted func(dir string, elsewhere bool) string
	// checkoutUnknown is set once the command line may have pointed git at a
	// repository the gate cannot name, or redefined a command the gate reads
	// (a function named `git` or `cd`; workdir.go). Unlike a directory, no
	// later `cd` or `-C` clears it: a push after it is refused wherever it
	// runs.
	checkoutUnknown bool
	// env is the environment a nested command line inherits from the command
	// that runs it (`bash -c`, `eval`): its opt-ins hold inside. nil at the
	// top.
	env map[string]string
}

// commandEnv is the environment a command runs with, as far as the gate reads
// it: what its command line inherited, then its prefix assignments.
func commandEnv(g gate, assigns []*syntax.Assign) map[string]string {
	env := make(map[string]string, len(g.env)+len(assigns))
	maps.Copy(env, g.env)
	for _, a := range assigns {
		if a.Name != nil {
			env[a.Name.Value] = literal(a.Value)
		}
	}
	return env
}

// nested is the gate, and the directory it starts in, for a command line run
// from dir by a command whose environment is env: it sees env's opt-ins, a
// repository variable in env holds for all of it, and it starts in an
// unknown directory when that variable is set or the command was moved
// (chdir: `env -C`).
func (g gate) nested(env map[string]string, chdir bool, dir wd) (gate, wd) {
	g.env = env
	repo := namesRepo(env)
	g.checkoutUnknown = g.checkoutUnknown || repo
	if (chdir || repo) && dir.state != dead {
		dir = unknownWD
	}
	return g, dir
}

func parse(cmd string) (*syntax.File, error) {
	return syntax.NewParser(syntax.Variant(syntax.LangBash)).Parse(strings.NewReader(cmd), "")
}

// literals is each argument's fixed text, "" where it depends on expansion.
func literals(call *syntax.CallExpr) []string {
	args := make([]string, 0, len(call.Args))
	for _, w := range call.Args {
		args = append(args, literal(w))
	}
	return args
}

// check returns why cmd is refused, or "" to allow it.
func check(cmd string, g gate) string {
	return checkIn(cmd, at(g.cwd), g)
}

// checkIn checks cmd started in dir: every command it runs, each in the
// directory it runs in (workdir.go).
func checkIn(cmd string, dir wd, g gate) string {
	file, err := parse(cmd)
	if err != nil {
		// Nothing in it can be followed. A push in it would go unchecked, so
		// text that mentions one is refused; the rest is let through, as the
		// gate cannot say what it runs.
		if g.unlinted != nil && mentionsPush(cmd) && !strings.Contains(cmd, unlintedOptIn+"=1") && g.env[unlintedOptIn] != "1" {
			return fmt.Sprintf(unreadablePushReason, fmt.Sprintf("this command — the bash parser cannot parse it (%v)", err))
		}
		return ""
	}
	w := walker{g: g}
	w.list(file.Stmts, dir)
	return w.reason
}

// checkCall checks one command. Its Stmt's redirections come with it: that is
// where a `python3 - <<'PY'` script lives.
func checkCall(call *syntax.CallExpr, redirs []*syntax.Redirect, dir wd, g gate) string {
	// The command's environment: what it inherited, its prefix assignments,
	// then env's.
	env := commandEnv(g, call.Assigns)
	args, chdir := unwrap(literals(call), env)
	if len(args) == 0 {
		return ""
	}

	switch args[0] {
	case "tst":
		if env[optIn] != "1" {
			return tstReason
		}
	case "cargo":
		// `tst` spelled out: its body run by hand is the same full suite, and
		// must not be the way around the opt-in.
		if wholeWorkspaceTest(args) && env[optIn] != "1" {
			return cargoTestReason
		}
	case "e2e":
		if env[optIn] != "1" {
			return e2eReason
		}
	case "sleep":
		if !g.background && len(args) > 1 {
			if d, ok := sleepDuration(args[1:]); ok && d >= 30*time.Second {
				return fmt.Sprintf(sleepReason, strings.Join(args[1:], " "))
			}
		}
	case "git", "gh":
		if pushes(args) && env[unlintedOptIn] != "1" && g.unlinted != nil {
			from := pushDir(args, dir)
			switch {
			case from.state == dead:
				return deadPushReason
			case chdir, g.checkoutUnknown, namesRepo(env):
				from = unknownWD
			}
			if from.state != known {
				return unknownDirReason
			}
			return g.unlinted(from.dir, filepath.Clean(from.dir) != filepath.Clean(g.cwd))
		}
	case "sed", "perl", "python", "python3":
		if scriptedEdit(args, redirs) && env[editOptIn] != "1" {
			return editReason
		}
	case "bash", "sh", "zsh":
		// `bash -c '<script>'` runs the script: check it as a command line,
		// run with this command's environment and from where it runs.
		for i, a := range args[:len(args)-1] {
			if a == "-c" || (strings.HasPrefix(a, "-") && strings.HasSuffix(a, "c") && !strings.HasPrefix(a, "--")) {
				ng, start := g.nested(env, chdir, dir)
				return checkIn(args[i+1], start, ng)
			}
		}
	}
	return ""
}

// pushes reports whether a git or gh command line pushes commits: `git push`
// (not a branch or tag deletion) and `gh stack submit`. `gh stack sync` is
// left alone — it rebases onto a fresh trunk and pushes in one step, so no
// lint could have seen its tree.
func pushes(args []string) bool {
	switch args[0] {
	case "git":
		i := parseGit(args).sub
		if i >= len(args) || args[i] != "push" {
			return false
		}
		for _, a := range args[i+1:] {
			if a == "--delete" || a == "-d" || a == "--tags" {
				return false
			}
		}
		return true
	case "gh":
		return len(args) > 2 && args[1] == "stack" && args[2] == "submit"
	}
	return false
}

// cargoValueFlags are cargo's global options whose value is the next argument.
var cargoValueFlags = map[string]bool{"--color": true, "--config": true, "-C": true, "-Z": true}

// wholeWorkspaceTest reports whether a cargo command line runs the tests of the
// whole workspace (`--workspace`, or its old spelling `--all`) — what `tst`
// runs — through `cargo test`, its alias `cargo t`, or `cargo nextest run`.
// `cargo test -p <crate>` is the ordinary narrow loop and passes, and so does
// `--no-run`, which compiles and runs nothing.
func wholeWorkspaceTest(args []string) bool {
	i := 1
	// `cargo +nightly test`, `cargo --locked test`, `cargo -C dir test`: skip
	// the toolchain and the global options, and the value of those that take
	// one.
	for i < len(args) && (strings.HasPrefix(args[i], "+") || strings.HasPrefix(args[i], "-")) {
		if cargoValueFlags[args[i]] {
			i++
		}
		i++
	}
	if i >= len(args) {
		return false
	}
	switch {
	case args[i] == "test" || args[i] == "t":
		i++
	case args[i] == "nextest" && i+1 < len(args) && args[i+1] == "run":
		i += 2
	default:
		return false
	}
	workspace := false
	for _, a := range args[i:] {
		if a == "--" {
			break
		}
		if a == "--no-run" {
			return false
		}
		if a == "--workspace" || a == "--all" {
			workspace = true
		}
	}
	return workspace
}

// unwrap strips wrappers that still run the command after them — `timeout
// 600 tst`, `env X=1 tst`, `devenv shell -- e2e` — collecting the assignments
// given to env into env (when it is not nil) on the way. chdir is set when
// env may run the command elsewhere: `env -C <dir>`, or `env -S <string>`,
// whose string can hold a -C of its own.
func unwrap(args []string, env map[string]string) (rest []string, chdir bool) {
	for len(args) > 0 {
		// `command -v tst` / `command -V tst` only look the name up.
		if args[0] == "command" && len(args) > 1 && (args[1] == "-v" || args[1] == "-V") {
			return nil, chdir
		}
		if n, ok := wrappers[args[0]]; ok {
			valueFlags := wrapperValueFlags[args[0]]
			isEnv := args[0] == "env"
			args = args[1:]
			for len(args) > 0 && strings.HasPrefix(args[0], "-") {
				opt := args[0]
				args = args[1:]
				if isEnv && !strings.HasPrefix(opt, "--") {
					next, elsewhere := envCluster(opt)
					chdir = chdir || elsewhere
					if next && len(args) > 0 {
						args = args[1:]
					}
					continue
				}
				flag, _, inline := strings.Cut(opt, "=")
				if isEnv && (flag == "--chdir" || flag == "--split-string") {
					chdir = true
				}
				if valueFlags[flag] && !inline && len(args) > 0 {
					args = args[1:]
				}
			}
			if len(args) < n {
				return nil, chdir
			}
			args = args[n:]
			for len(args) > 0 && strings.Contains(args[0], "=") {
				if name, value, _ := strings.Cut(args[0], "="); env != nil {
					env[name] = value
				}
				args = args[1:]
			}
			continue
		}
		if args[0] == "devenv" && len(args) > 1 && args[1] == "shell" {
			args = args[2:]
			if len(args) > 0 && args[0] == "--" {
				args = args[1:]
			}
			continue
		}
		break
	}
	return args, chdir
}

// envCluster reads a cluster of env's short options: `-i`, `-iC <dir>`,
// `-iC<dir>`, `-u X`. The first option that takes a value takes the rest of
// the cluster, or the next argument when nothing is left (next). elsewhere is
// set when the cluster holds a C or an S anywhere — `-C`, or `-S`, whose
// string can hold a -C — as the gate does not tell an option from a value
// inside one (`-uC` unsets C, and is still read as a move).
func envCluster(opt string) (next, elsewhere bool) {
	elsewhere = strings.ContainsAny(opt[1:], "CS")
	for i := 1; i < len(opt); i++ {
		if strings.IndexByte("CSua", opt[i]) >= 0 {
			return i == len(opt)-1, elsewhere
		}
	}
	return false, elsewhere
}

// literal returns a word's value when it is fixed text — bare, 'single' or
// "double" quoted without expansions — and "" when it depends on expansion.
func literal(w *syntax.Word) string {
	if w == nil {
		return ""
	}
	var b strings.Builder
	for _, part := range w.Parts {
		switch p := part.(type) {
		case *syntax.Lit:
			b.WriteString(p.Value)
		case *syntax.SglQuoted:
			b.WriteString(p.Value)
		case *syntax.DblQuoted:
			for _, q := range p.Parts {
				lit, ok := q.(*syntax.Lit)
				if !ok {
					return ""
				}
				b.WriteString(lit.Value)
			}
		default:
			return ""
		}
	}
	return b.String()
}

// sleepDuration sums sleep's operands, which GNU sleep accepts with an
// s/m/h/d suffix and a fractional part. ok is false for anything else.
func sleepDuration(operands []string) (time.Duration, bool) {
	units := map[byte]float64{'s': 1, 'm': 60, 'h': 3600, 'd': 86400}
	var total time.Duration
	for _, op := range operands {
		mult := 1.0
		if op != "" {
			if u, ok := units[op[len(op)-1]]; ok {
				mult, op = u, op[:len(op)-1]
			}
		}
		n, err := strconv.ParseFloat(op, 64)
		if err != nil || n < 0 {
			return 0, false
		}
		total += time.Duration(n * mult * float64(time.Second))
	}
	return total, true
}
