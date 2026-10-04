package main

import (
	"regexp"
	"strings"

	"mvdan.cc/sh/v3/syntax"
)

// scriptedEdit reports whether a sed, perl or python command line rewrites a
// file in place. args are the command's literal arguments, redirs the
// redirections of its statement (a heredoc script lives there).
//
// Only the in-place forms count: `sed -n 'A,Bp'` is the narrow read CLAUDE.md
// asks for, and a python script that reads a transcript and prints numbers is
// analysis, not an edit.
func scriptedEdit(args []string, redirs []*syntax.Redirect) bool {
	switch args[0] {
	case "sed":
		return sedInPlace(args[1:])
	case "perl":
		return perlInPlace(args[1:])
	default:
		return pythonRewrites(pythonScript(args[1:], redirs))
	}
}

// sedInPlace: `-i`, `-i.bak`, `--in-place[=SUF]`, or a short cluster holding
// i (`-Ei`, `-ni`). Short options that take a value (-e, -f, -l) are separate
// words here, and a script never starts with a dash.
func sedInPlace(args []string) bool {
	for _, a := range args {
		switch {
		case a == "--":
			return false
		case strings.HasPrefix(a, "--in-place"):
			return true
		case strings.HasPrefix(a, "--"):
		case strings.HasPrefix(a, "-") && strings.Contains(a, "i"):
			return true
		}
	}
	return false
}

// perlInPlace: an `i` in a switch cluster before any switch that consumes the
// rest of the word as its value (`-pi`, `-0777 -pi`, `-i.bak`, but not
// `-Mlib=include` or `-e 'print "i"'`).
func perlInPlace(args []string) bool {
	for _, a := range args {
		if a == "--" || !strings.HasPrefix(a, "-") || strings.HasPrefix(a, "--") {
			continue
		}
	cluster:
		for j := 1; j < len(a); j++ {
			switch c := a[j]; {
			case c == 'i':
				return true
			case c == '0' || c == 'l':
				// -0[octal], -l[octal]: the digits are the value, switches may follow.
				for j+1 < len(a) && a[j+1] >= '0' && a[j+1] <= '9' {
					j++
				}
			case strings.IndexByte("eEMmIxCdDV", c) >= 0:
				break cluster
			}
		}
	}
	return false
}

// pythonScript is the program text a python command line runs: its `-c`
// argument, or a heredoc fed to it. A script file is not read.
func pythonScript(args []string, redirs []*syntax.Redirect) string {
	var b strings.Builder
	for i, a := range args {
		if a == "-c" && i+1 < len(args) {
			b.WriteString(args[i+1])
		}
	}
	for _, r := range redirs {
		if r.Op == syntax.Hdoc || r.Op == syntax.DashHdoc {
			b.WriteString(rawText(r.Hdoc))
		}
	}
	return b.String()
}

var (
	pyReads    = regexp.MustCompile(`\.read\(|\.read_text\(|\.readlines\(`)
	pyReplaces = regexp.MustCompile(`\.replace\(|\bre\.subn?\(|\.sub\(`)
	pyWrites   = regexp.MustCompile(`\.write\(|\.write_text\(|\.writelines\(`)
)

// pythonRewrites: the read-replace-write shape of an edit. Each part alone is
// ordinary — reading a file, string surgery on output, writing a report.
func pythonRewrites(script string) bool {
	return pyReads.MatchString(script) && pyReplaces.MatchString(script) && pyWrites.MatchString(script)
}

// rawText is a word's literal text with its expansions left out — enough to
// read a heredoc script whose delimiter was not quoted.
func rawText(w *syntax.Word) string {
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
				if lit, ok := q.(*syntax.Lit); ok {
					b.WriteString(lit.Value)
				}
			}
		}
	}
	return b.String()
}
