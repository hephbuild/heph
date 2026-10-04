package main

import "testing"

// The forms the 2026 sessions actually used, and the reads that must pass:
// `sed -n` range reads and transcript-analysis scripts are what CLAUDE.md asks
// for, so a gate that fires on them teaches the agent to route around it.

func TestBlocksScriptedEdits(t *testing.T) {
	for _, cmd := range []string{
		"sed -i 's/a/b/' src/x.rs",
		"sed -i.bak 's/a/b/' src/x.rs",
		"sed -Ei 's/a/b/' src/x.rs",
		"sed --in-place 's/a/b/' src/x.rs",
		"sed --in-place=.orig -e 's/a/b/' src/x.rs",
		"sed -i '312s/foo/bar/' crates/engine/src/engine/result.rs && cargo check",
		"cd crates && sed -i 's/a/b/' x.rs",
		"timeout 10 sed -i 's/a/b/' x.rs",
		"perl -pi -e 's/a/b/' x.rs",
		"perl -0777 -pi -e 's/a\\nb/c/' x.rs",
		"perl -0pi -e 's/a/b/' x.rs",
		"perl -i.bak -pe 's/a/b/' x.rs",
		"perl -lpi -e 's/a/b/' x.rs",
		"perl -p -i -e 's/a/b/' x.rs",
		"python3 - <<'PY'\nimport io\np='src/x.rs'\ns=io.open(p).read()\nold='a'\nassert s.count(old)==1\ns=s.replace(old,'b')\nio.open(p,'w').write(s)\nPY",
		"python3 - <<PY\nfrom pathlib import Path\np=Path('$F')\np.write_text(p.read_text().replace('a','b'))\nPY",
		"python3 <<-'EOF'\n\timport re\n\ts=open('x').read()\n\ts=re.sub('a','b',s)\n\topen('x','w').write(s)\n\tEOF",
		"python -c \"s=open('x').read(); open('x','w').write(s.replace('a','b'))\"",
		"bash -c \"sed -i 's/a/b/' x\"",
	} {
		if check(cmd, gate{}) == "" {
			t.Errorf("should block %q", cmd)
		}
	}
}

func TestAllowsReadsAndOptedInEdits(t *testing.T) {
	for _, cmd := range []string{
		"sed -n '120,180p' crates/engine/src/engine/result.rs",
		"sed -e 's/a/b/' x.rs > out",
		"sed 's/-i/x/' x.rs",
		"rg -l foo | xargs echo sed -i",
		"git grep -n 'sed -i'",
		"perl -ne 'print if /i/' x",
		"perl -Mlib=include -e 'print 1'",
		"perl -e 'print \"-i\"'",
		// Analysis: reads and prints, or writes a report without rewriting.
		"python3 - <<'EOF'\nimport json\nfor l in open('t.jsonl'):\n  d=json.loads(l)\nprint(d)\nEOF",
		"python3 - <<'EOF'\ns=open('t').read()\nprint(s.replace('a','b'))\nEOF",
		"python3 -c \"import json,sys; print(json.load(sys.stdin)['x'])\"",
		"python3 - <<'EOF'\nimport json\nout=json.dumps({'a':1}).replace(' ','')\nopen('report.json','w').write(out)\nEOF",
		"python3 script.py",
		"HEPH_SCRIPTED_EDIT=1 sed -i '12s/f(a)/f(a, X)/' src/x.rs && git diff -U0",
		"env HEPH_SCRIPTED_EDIT=1 perl -pi -e 's/a/b/' x.rs",
		"HEPH_SCRIPTED_EDIT=1 python3 - <<'PY'\ns=open('x').read()\nopen('x','w').write(s.replace('a','b'))\nPY",
	} {
		if r := check(cmd, gate{}); r != "" {
			t.Errorf("should allow %q, got: %s", cmd, r)
		}
	}
}
