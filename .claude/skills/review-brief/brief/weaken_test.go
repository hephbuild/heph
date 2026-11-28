package main

import (
	"strings"
	"testing"
)

const testsBase = `pub fn f() -> u32 { 1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps() {
        assert_eq!(f(), 1);
        assert!(f() > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goes_away() {
        assert_eq!(f(), 1);
    }
}
`

func weakeningsFor(t *testing.T, after map[string]string) []Weakening {
	_, st := oneLayer(t, map[string]string{"src/lib.rs": testsBase}, after)
	return st.Weakenings(st.Layers[0])
}

func kinds(ws []Weakening) map[string]string {
	out := map[string]string{}
	for _, w := range ws {
		out[w.Test] = w.Kind
	}
	return out
}

func Test_deleted_test_is_reported(t *testing.T) {
	after := strings.Replace(testsBase, "    #[tokio::test(flavor = \"multi_thread\")]\n    async fn goes_away() {\n        assert_eq!(f(), 1);\n    }\n", "", 1)
	ws := weakeningsFor(t, map[string]string{"src/lib.rs": after})
	if kinds(ws)["goes_away"] != "deleted-test" {
		t.Fatalf("weakenings %+v, want goes_away deleted", ws)
	}
	w := ws[0]
	if w.Range == nil || w.Range.Side != SideOld || w.Range.Path != "src/lib.rs" {
		t.Fatalf("deleted test's range %+v, want an old-side range in src/lib.rs", w.Range)
	}
}

func Test_new_ignore_is_reported(t *testing.T) {
	after := strings.Replace(testsBase, "    #[test]\n    fn keeps()", "    #[test]\n    #[ignore = \"flaky\"]\n    fn keeps()", 1)
	ws := weakeningsFor(t, map[string]string{"src/lib.rs": after})
	if kinds(ws)["keeps"] != "new-ignore" {
		t.Fatalf("weakenings %+v, want keeps newly ignored", ws)
	}
}

func Test_new_test_added_ignored_is_reported(t *testing.T) {
	after := strings.Replace(testsBase, "}\n", "}\n", 1) + "#[test]\n#[ignore]\nfn never_ran() { assert!(false); }\n"
	ws := weakeningsFor(t, map[string]string{"src/lib.rs": after})
	if kinds(ws)["never_ran"] != "new-ignore" {
		t.Fatalf("weakenings %+v, want never_ran reported", ws)
	}
}

func Test_renamed_test_is_not_reported(t *testing.T) {
	// The test function renamed with its body kept, and the other test
	// moved to another file: neither is a deletion.
	moved := "#[tokio::test(flavor = \"multi_thread\")]\nasync fn goes_away() {\n    assert_eq!(super::f(), 1);\n}\n"
	after := strings.Replace(testsBase, "fn keeps()", "fn keeps_renamed()", 1)
	after = strings.Replace(after, "    #[tokio::test(flavor = \"multi_thread\")]\n    async fn goes_away() {\n        assert_eq!(f(), 1);\n    }\n", "", 1)
	ws := weakeningsFor(t, map[string]string{"src/lib.rs": after, "src/other.rs": moved})
	if len(ws) != 0 {
		t.Fatalf("weakenings %+v, want none", ws)
	}
}

func Test_lost_assertions_reported(t *testing.T) {
	after := strings.Replace(testsBase, "        assert!(f() > 0);\n", "", 1)
	ws := weakeningsFor(t, map[string]string{"src/lib.rs": after})
	if kinds(ws)["keeps"] != "lost-assertions" {
		t.Fatalf("weakenings %+v, want keeps to have lost an assertion", ws)
	}
	if ws[0].Range.Side != SideOld {
		t.Fatalf("lost assertions anchor on the old side, where the removed lines are: %+v", ws[0].Range)
	}
}

func Test_go_test_skip_and_delete(t *testing.T) {
	base := "package x\n\nimport \"testing\"\n\nfunc TestA(t *testing.T) {\n\tif 1 != 1 {\n\t\tt.Fatal(\"no\")\n\t}\n}\n\nfunc TestB(t *testing.T) {\n\tt.Error(\"b\")\n}\n"
	after := "package x\n\nimport \"testing\"\n\nfunc TestA(t *testing.T) {\n\tt.Skip(\"later\")\n\tif 1 != 1 {\n\t\tt.Fatal(\"no\")\n\t}\n}\n"
	_, st := oneLayer(t, map[string]string{"x/x_test.go": base}, map[string]string{"x/x_test.go": after})
	k := kinds(st.Weakenings(st.Layers[0]))
	if k["TestA"] != "new-ignore" || k["TestB"] != "deleted-test" {
		t.Fatalf("got %v, want TestA skipped and TestB deleted", k)
	}
}

func Test_missing_spec_test_reported(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/lib.rs": "\n"}, map[string]string{"src/lib.rs": testsBase})
	ws := st.MissingSpecTests([]string{"keeps", "goes_away", "never_written"})
	if len(ws) != 1 || ws[0].Test != "never_written" {
		t.Fatalf("missing spec tests %+v, want only never_written", ws)
	}
}

func Test_macro_generated_named_test_exists(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/lib.rs": "\n"},
		map[string]string{"src/lib.rs": "macro_rules! case { ($n:ident) => { #[test] fn $n() {} } }\ncase!(macro_generated_case);\n"})
	found, ignored := st.testExists(st.Layers[0].Head, TestRef{Name: "macro_generated_case"})
	if !found || ignored {
		t.Fatalf("found %v ignored %v, want a macro-generated test found", found, ignored)
	}
}
