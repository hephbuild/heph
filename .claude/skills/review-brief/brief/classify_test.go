package main

import (
	"fmt"
	"os"
	"strings"
	"testing"
)

func block(prefix string, n int) string {
	return lines(n, func(i int) string {
		return fmt.Sprintf("    let %s_value_%d = compute_something_long(argument_%d, other);", prefix, i, i)
	})
}

// A base commit on origin/master and one layer on top of it.
func oneLayer(t *testing.T, before, after map[string]string) (*fixture, *Stack) {
	f := newFixture(t)
	for p, b := range before {
		f.write(p, b)
	}
	base := f.commit("base")
	f.trunk(base)
	for p, b := range after {
		if b == "" {
			f.rm(p)
		} else {
			f.write(p, b)
		}
	}
	head := f.commit("layer")
	return f, f.stack(ModeWhole, head)
}

func Test_line_replaced_by_higher_layer_is_counted(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn a() {}\nfn keep() {}\nfn replaced() {}\n")
	h1 := f.commit("layer 1")
	f.write("src/a.rs", "fn a() {}\nfn keep() {}\nfn replacement() {}\n")
	h2 := f.commit("layer 2")
	st := f.stack(ModeWhole, h1, h2)
	l1, l2 := st.Layers[0], st.Layers[1]
	if c := classOfText(t, l1, SideNew, "fn replaced() {}"); c != ClassSuperseded {
		t.Errorf("layer 1's replaced line: %s, want superseded", c)
	}
	if c := classOfText(t, l1, SideNew, "fn keep() {}"); c != ClassLive {
		t.Errorf("layer 1's kept line: %s, want live", c)
	}
	if c := classOfText(t, l2, SideNew, "fn replacement() {}"); c != ClassLive {
		t.Errorf("layer 2's line: %s, want live", c)
	}
	// Layer 2 removing a line only layer 1 added never reaches master.
	if c := classOfText(t, l2, SideOld, "fn replaced() {}"); c != ClassSuperseded {
		t.Errorf("layer 2's removal of layer 1's line: %s, want superseded", c)
	}
}

func Test_per_layer_mode_shows_superseded(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn a() {}\nfn replaced() {}\n")
	h1 := f.commit("layer 1")
	f.write("src/a.rs", "fn a() {}\nfn replacement() {}\n")
	h2 := f.commit("layer 2")
	st := f.stack(ModePerLayer, h1, h2)
	if c := classOfText(t, st.Layers[0], SideNew, "fn replaced() {}"); c != ClassLive {
		t.Errorf("per-layer mode: %s, want live (that state reaches master)", c)
	}
}

func Test_three_layer_touch_ownership(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\nfn z() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn a() {}\nfn x() {}\nfn z() {}\n")
	h1 := f.commit("layer 1 adds x")
	f.write("src/a.rs", "fn a() {}\nfn z() {}\n")
	h2 := f.commit("layer 2 deletes x")
	f.write("src/a.rs", "fn a() {}\nfn x() {}\nfn z() {}\n")
	h3 := f.commit("layer 3 adds x again")
	st := f.stack(ModeWhole, h1, h2, h3)
	if c := classOfText(t, st.Layers[0], SideNew, "fn x() {}"); c != ClassSuperseded {
		t.Errorf("layer 1's x: %s, want superseded (layer 3 owns the final line)", c)
	}
	if c := classOfText(t, st.Layers[1], SideOld, "fn x() {}"); c != ClassSuperseded {
		t.Errorf("layer 2's deletion: %s, want superseded", c)
	}
	if c := classOfText(t, st.Layers[2], SideNew, "fn x() {}"); c != ClassLive {
		t.Errorf("layer 3's x: %s, want live", c)
	}
}

func Test_rename_then_edit_supersession(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", block("base", 12))
	f.trunk(f.commit("base"))
	body := block("base", 12) + "fn added_one() {}\nfn added_two() {}\n"
	f.write("src/a.rs", body)
	h1 := f.commit("layer 1")
	f.rm("src/a.rs")
	f.write("src/b.rs", strings.Replace(body, "fn added_two() {}", "fn added_two_edited() {}", 1))
	h2 := f.commit("layer 2 renames and edits")
	st := f.stack(ModeWhole, h1, h2)
	l1 := st.Layers[0]
	if c := classOfText(t, l1, SideNew, "fn added_one() {}"); c != ClassLive {
		t.Errorf("line kept through the rename: %s, want live", c)
	}
	if c := classOfText(t, l1, SideNew, "fn added_two() {}"); c != ClassSuperseded {
		t.Errorf("line edited after the rename: %s, want superseded", c)
	}
}

func Test_bottom_base_is_merge_base_with_origin_master(t *testing.T) {
	f := newFixture(t)
	f.write("a.txt", "1\n")
	m1 := f.commit("m1")
	f.write("a.txt", "1\n2\n")
	m2 := f.commit("m2")
	f.trunk(m2)
	f.git("checkout", "-q", "-b", "feat", m2)
	f.git("branch", "-f", "master", m1) // a stale local master
	f.write("b.txt", "feature\n")
	h := f.commit("feature")
	st := f.stack(ModeWhole, h)
	if got := st.Layers[0].Base; got != m2 {
		t.Fatalf("bottom base %s, want origin/master's %s", short(got), short(m2))
	}
	if n := len(st.Layers[0].Lines()); n != 1 {
		t.Fatalf("layer has %d changed lines, want 1: the stale master widened the range", n)
	}
}

func Test_layer_not_on_top_of_the_one_below_is_refused(t *testing.T) {
	f := newFixture(t)
	f.write("a.txt", "1\n")
	base := f.commit("base")
	f.trunk(base)
	f.write("a.txt", "2\n")
	h1 := f.commit("layer 1")
	f.git("checkout", "-q", "-b", "other", base)
	f.write("b.txt", "x\n")
	h2 := f.commit("not stacked")
	_, err := pinLayers(f.g, "o/r", "origin/master", ModeWhole, []Layer{{PR: 1, Head: h1}, {PR: 2, Head: h2}})
	if err == nil || !strings.Contains(err.Error(), "restack") {
		t.Fatalf("want a restack error, got %v", err)
	}
}

func Test_rebase_onto_master_keeps_layer_fingerprint(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	m1 := f.commit("m1")
	f.trunk(m1)
	f.write("src/a.rs", "fn a() {}\nfn layer() {}\n")
	h1 := f.commit("layer")
	before := f.stack(ModeWhole, h1).Layers[0].Fingerprint()

	f.git("checkout", "-q", "-b", "m2", m1)
	f.write("other.rs", "fn unrelated() {}\n")
	m2 := f.commit("m2")
	f.trunk(m2)
	f.write("src/a.rs", "fn a() {}\nfn layer() {}\n")
	h2 := f.commit("layer, rebased")
	after := f.stack(ModeWhole, h2).Layers[0].Fingerprint()
	if before != after {
		t.Fatalf("fingerprint moved on a rebase: %s -> %s", before, after)
	}

	f.write("src/a.rs", "fn a() {}\nfn layer_changed() {}\n")
	h3 := f.commit("layer, edited")
	if f.stack(ModeWhole, h3).Layers[0].Fingerprint() == before {
		t.Fatal("fingerprint did not move when the layer's content changed")
	}
}

// The adversarial inputs table.

func Test_test_module_file_declared_elsewhere_is_test(t *testing.T) {
	for _, layout := range []string{"src/foo/tests.rs", "src/foo/tests/mod.rs"} {
		t.Run(layout, func(t *testing.T) {
			_, st := oneLayer(t, map[string]string{"src/lib.rs": "mod foo;\n", "src/foo.rs": "pub fn f() {}\n"},
				map[string]string{
					"src/foo.rs": "pub fn f() {}\n\n#[cfg(test)]\nmod tests;\n",
					layout:       "use super::*;\n#[test]\nfn works() { assert!(true); }\n",
				})
			d := st.Layers[0]
			if c := classOfText(t, d, SideNew, "fn works() { assert!(true); }"); c != ClassTest {
				t.Errorf("%s: %s, want test", layout, c)
			}
			if c := classOfText(t, d, SideNew, "mod tests;"); c != ClassTest {
				t.Errorf("the declaration itself: %s, want test", c)
			}
		})
	}
}

func Test_submodule_of_test_module_is_test(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/lib.rs": "pub fn f() {}\n"},
		map[string]string{
			"src/lib.rs":        "pub fn f() {}\n#[cfg(test)]\nmod tests;\n",
			"src/tests/mod.rs":  "mod deep;\n",
			"src/tests/deep.rs": "#[test]\nfn deep() {}\n",
		})
	if c := classOfText(t, st.Layers[0], SideNew, "fn deep() {}"); c != ClassTest {
		t.Errorf("a module of a test module: %s, want test", c)
	}
}

func Test_cfg_test_item_in_live_code_is_only_that_item(t *testing.T) {
	src := `pub fn live_before() {}

#[cfg(test)]
fn helper() -> u32 {
    1
}

pub fn live_after() {}
`
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	d := st.Layers[0]
	for text, want := range map[string]Class{
		"pub fn live_before() {}": ClassLive,
		"fn helper() -> u32 {":    ClassTest,
		"    1":                   ClassTest,
		"pub fn live_after() {}":  ClassLive,
	} {
		if c := classOfText(t, d, SideNew, text); c != want {
			t.Errorf("%q: %s, want %s", text, c, want)
		}
	}
}

func Test_cfg_predicates(t *testing.T) {
	for pred, want := range map[string]bool{
		"test":                                true,
		"all(test, unix)":                     true,
		"any(test, test)":                     true,
		`any(test, feature = "test-support")`: false,
		"not(test)":                           false,
		"unix":                                false,
		`all(unix, feature = "x")`:            false,
	} {
		if got := cfgTestOnly(pred); got != want {
			t.Errorf("cfg(%s): %v, want %v", pred, got, want)
		}
	}
	if attrCfgTest(`cfg_attr(test, derive(Debug))`) {
		t.Error("cfg_attr is not a test gate")
	}
}

func Test_cfg_any_test_feature_is_live(t *testing.T) {
	src := "#[cfg(any(test, feature = \"test-support\"))]\npub fn support() {}\n#[cfg(not(test))]\npub fn real() {}\n#[cfg_attr(test, derive(Debug))]\npub struct S;\n"
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	for _, text := range []string{"pub fn support() {}", "pub fn real() {}", "pub struct S;"} {
		if c := classOfText(t, st.Layers[0], SideNew, text); c != ClassLive {
			t.Errorf("%q: %s, want live", text, c)
		}
	}
}

func Test_lexer_traps_do_not_end_a_test_module(t *testing.T) {
	src := `pub fn live() {}

#[cfg(test)]
mod tests {
    const RAW: &str = r#"
}
"#;
    fn brace() -> char { '{' }
    fn lifetime<'a>(s: &'a str) -> &'a str { s }
    fn escaped() -> char { '\'' }
    /* a } in a /* nested */ comment */
    // and a } in a line comment
    #[test]
    fn still_inside() {}
}

pub fn live_after() {}
`
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	d := st.Layers[0]
	if c := classOfText(t, d, SideNew, "    fn still_inside() {}"); c != ClassTest {
		t.Errorf("inside the module: %s, want test", c)
	}
	if c := classOfText(t, d, SideNew, "pub fn live_after() {}"); c != ClassLive {
		t.Errorf("after the module: %s, want live", c)
	}
}

func Test_cfg_test_field_and_statement(t *testing.T) {
	src := `pub struct S {
    a: u32,
    #[cfg(test)]
    walked: usize,
    b: u32,
}

pub fn f(s: &mut S) {
    #[cfg(test)]
    {
        s.walked += 1;
    }
    s.a += 1;
}
`
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	d := st.Layers[0]
	for text, want := range map[string]Class{
		"    walked: usize,":     ClassTest,
		"    b: u32,":            ClassLive,
		"        s.walked += 1;": ClassTest,
		"    s.a += 1;":          ClassLive,
	} {
		if c := classOfText(t, d, SideNew, text); c != want {
			t.Errorf("%q: %s, want %s", text, c, want)
		}
	}
}

func Test_test_region_to_eof_flagged(t *testing.T) {
	src := "pub fn live() {}\n\n#[cfg(test)]\nmod tests {\n    fn never_closed() {}\n\npub fn swallowed() {}\n"
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	cl := lineAt(t, st.Layers[0], SideNew, "pub fn swallowed() {}")
	if cl.Class != ClassLive || !hasFlag(cl, "untrusted-test-region") {
		t.Fatalf("a region to the end of the file: class %s flags %v, want live and flagged", cl.Class, cl.Flags)
	}
}

func Test_test_region_closing_at_another_indent_is_flagged(t *testing.T) {
	src := "#[cfg(test)]\nmod tests {\n    fn x() {}\n    }\n"
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": src})
	cl := lineAt(t, st.Layers[0], SideNew, "    fn x() {}")
	if cl.Class != ClassLive || !hasFlag(cl, "untrusted-test-region") {
		t.Fatalf("class %s flags %v, want live and flagged", cl.Class, cl.Flags)
	}
}

func hasFlag(cl *CLine, f string) bool {
	for _, x := range cl.Flags {
		if x == f {
			return true
		}
	}
	return false
}

func Test_harness_files_are_glance_not_counted(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"x/tests/fault_provider.rs": "\n", "testkit/lib.rs": "\n"},
		map[string]string{"x/tests/fault_provider.rs": "pub fn fail() {}\n", "testkit/lib.rs": "pub fn kit() {}\n"})
	for _, text := range []string{"pub fn fail() {}", "pub fn kit() {}"} {
		cl := lineAt(t, st.Layers[0], SideNew, text)
		if cl.Class != ClassLive || cl.Floor != TierGlance {
			t.Errorf("%q: class %s floor %s, want live at Glance", text, cl.Class, cl.Floor)
		}
	}
}

func Test_build_file_under_testdata_is_test(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"pkg/testdata/BUILD": "\n"}, map[string]string{"pkg/testdata/BUILD": "target(name = \"x\")\n"})
	if c := classOfText(t, st.Layers[0], SideNew, "target(name = \"x\")"); c != ClassTest {
		t.Fatalf("%s, want test", c)
	}
}

func Test_moved_block_across_files_is_counted(t *testing.T) {
	b := block("m", 10)
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "src/b.rs": "\n"}, map[string]string{"src/a.rs": "\n", "src/b.rs": "\n" + b})
	d := st.Layers[0]
	first := strings.SplitN(b, "\n", 2)[0]
	add, rem := lineAt(t, d, SideNew, first), lineAt(t, d, SideOld, first)
	if add.Class != ClassMoved || rem.Class != ClassMoved {
		t.Fatalf("moved block: added %s removed %s, want moved both", add.Class, rem.Class)
	}
	if add.Other == nil || add.Other.Path != "src/a.rs" {
		t.Fatalf("moved line's source: %+v, want src/a.rs", add.Other)
	}
}

func Test_short_move_is_live(t *testing.T) {
	b := block("m", 7) // one line under the threshold
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "src/b.rs": "\n"}, map[string]string{"src/a.rs": "\n", "src/b.rs": "\n" + b})
	if c := classOfText(t, st.Layers[0], SideNew, strings.SplitN(b, "\n", 2)[0]); c != ClassLive {
		t.Fatalf("a 7-line move: %s, want live", c)
	}
}

func Test_test_to_live_move_is_live(t *testing.T) {
	b := block("m", 10)
	_, st := oneLayer(t, map[string]string{"x/tests/a.rs": b, "src/b.rs": "\n"}, map[string]string{"x/tests/a.rs": "\n", "src/b.rs": "\n" + b})
	if c := classOfText(t, st.Layers[0], SideNew, strings.SplitN(b, "\n", 2)[0]); c != ClassLive {
		t.Fatalf("code moved out of a test: %s, want live", c)
	}
}

func Test_intra_file_reorder_is_glance(t *testing.T) {
	a, b := block("first", 10), block("second", 10)
	_, st := oneLayer(t, map[string]string{"src/a.rs": a + b}, map[string]string{"src/a.rs": b + a})
	d := st.Layers[0]
	flagged := 0
	for _, cl := range d.Lines() {
		if hasFlag(cl, "reorder") {
			flagged++
			if cl.Class != ClassLive || cl.Floor != TierGlance {
				t.Fatalf("reordered line %q: class %s floor %s, want live at Glance", cl.Text, cl.Class, cl.Floor)
			}
		}
	}
	if flagged == 0 {
		t.Fatal("no line was flagged as a reorder")
	}
}

func Test_move_into_protected_path_is_glance(t *testing.T) {
	b := block("m", 10)
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "core/b.rs": "\n"}, map[string]string{"src/a.rs": "\n", "core/b.rs": "\n" + b})
	cl := lineAt(t, st.Layers[0], SideNew, strings.SplitN(b, "\n", 2)[0])
	if cl.Class != ClassLive || cl.Floor != TierGlance || !hasFlag(cl, "moved-into-protected") {
		t.Fatalf("class %s floor %s flags %v", cl.Class, cl.Floor, cl.Flags)
	}
}

func Test_moved_block_with_edited_middle_line(t *testing.T) {
	b := block("m", 20)
	ls := strings.Split(strings.TrimSuffix(b, "\n"), "\n")
	edited := append([]string{}, ls...)
	edited[10] = "    let edited_in_the_middle = 1;"
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "src/b.rs": "\n"},
		map[string]string{"src/a.rs": "\n", "src/b.rs": "\n" + strings.Join(edited, "\n") + "\n"})
	d := st.Layers[0]
	if c := classOfText(t, d, SideNew, "    let edited_in_the_middle = 1;"); c != ClassLive {
		t.Errorf("the edited line: %s, want live", c)
	}
	if c := classOfText(t, d, SideNew, ls[0]); c != ClassMoved {
		t.Errorf("the first half: %s, want moved", c)
	}
	if c := classOfText(t, d, SideNew, ls[19]); c != ClassMoved {
		t.Errorf("the second half: %s, want moved", c)
	}
}

func Test_block_removed_once_added_twice(t *testing.T) {
	b := block("m", 10)
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "src/b.rs": "\n", "src/c.rs": "\n"},
		map[string]string{"src/a.rs": "\n", "src/b.rs": "\n" + b, "src/c.rs": "\n" + b})
	first := strings.SplitN(b, "\n", 2)[0]
	moved, live := 0, 0
	for _, cl := range st.Layers[0].Lines() {
		if cl.Side == SideNew && cl.Text == first {
			switch cl.Class {
			case ClassMoved:
				moved++
			case ClassLive:
				live++
			}
		}
	}
	if moved != 1 || live != 1 {
		t.Fatalf("moved %d live %d, want one of each", moved, live)
	}
}

func Test_rename_with_edits_and_protected_deletion(t *testing.T) {
	b := block("r", 12)
	_, st := oneLayer(t, map[string]string{"src/a.rs": b, "core/gone.rs": "pub fn gone() {}\n"},
		map[string]string{"src/a.rs": "", "src/renamed.rs": b + "fn edit() {}\n", "core/gone.rs": ""})
	d := st.Layers[0]
	if c := classOfText(t, d, SideNew, "fn edit() {}"); c != ClassLive {
		t.Errorf("the edit in a rename: %s, want live", c)
	}
	if n := len(d.Lines()); n != 2 {
		t.Errorf("%d changed lines, want 2 (the rename's edit and the deletion)", n)
	}
	cl := lineAt(t, d, SideOld, "pub fn gone() {}")
	if cl.Class != ClassLive || cl.Floor != TierGlance {
		t.Errorf("deletion in a protected path: class %s floor %s, want live at Glance", cl.Class, cl.Floor)
	}
}

func Test_binary_and_non_utf8_counted_by_name(t *testing.T) {
	f, st := oneLayer(t, map[string]string{"x.txt": "\n"}, map[string]string{
		"img.bin":    "\x00\x01\x02binary\x00",
		"latin1.txt": "caf\xe9\n",
	})
	_ = f
	d := st.Layers[0]
	var bin, lat *LFile
	for _, file := range d.Files {
		switch file.Path {
		case "img.bin":
			bin = file
		case "latin1.txt":
			lat = file
		}
	}
	if bin == nil || bin.FileClass != ClassBinary {
		t.Fatalf("binary file: %+v", bin)
	}
	if lat == nil || lat.FileClass != ClassBinary {
		t.Fatalf("non-UTF-8 file: %+v", lat)
	}
	rows := st.counted(d)
	names := map[string]bool{}
	for _, r := range rows {
		names[r.Path] = true
	}
	if !names["img.bin"] || !names["latin1.txt"] {
		t.Fatalf("counted rows %v miss a binary or non-UTF-8 file", rows)
	}
}

func Test_mode_symlink_change_listed(t *testing.T) {
	f := newFixture(t)
	f.write("run.sh", "echo hi\n")
	f.write("target.txt", "t\n")
	f.trunk(f.commit("base"))
	if err := os.Chmod(f.dir+"/run.sh", 0o755); err != nil {
		t.Fatal(err)
	}
	if err := symlink("target.txt", f.dir+"/link"); err != nil {
		t.Fatal(err)
	}
	h := f.commit("symlink")
	st := f.stack(ModeWhole, h)
	rows := st.counted(st.Layers[0])
	got := map[string]Class{}
	for _, r := range rows {
		got[r.Path] = r.Class
	}
	if got["run.sh"] != ClassMode {
		t.Errorf("mode change: %q, want listed as mode (rows %v)", got["run.sh"], rows)
	}
	if got["link"] != ClassSymlink {
		t.Errorf("symlink: %q, want listed as symlink (rows %v)", got["link"], rows)
	}
}

func Test_long_line_crlf_no_eol_and_odd_paths(t *testing.T) {
	long := strings.Repeat("x", 70*1024)
	_, st := oneLayer(t, map[string]string{"dir with space/é.rs": "a\r\nb\r\n"},
		map[string]string{"dir with space/é.rs": "a\r\nc\r\n" + long + "\n" + "no newline"})
	d := st.Layers[0]
	if d.File("dir with space/é.rs", SideNew) == nil {
		t.Fatal("path with a space and non-ASCII was not read exactly")
	}
	if n := len(d.Lines()); n != d.Numstat {
		t.Fatalf("%d lines classified, numstat %d", n, d.Numstat)
	}
	lineAt(t, d, SideNew, long)
	lineAt(t, d, SideNew, "c\r")
	lineAt(t, d, SideNew, "no newline")
}

func Test_comment_only_change_in_protected_file_is_live(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"core/a.rs": "// old contract\npub fn f() {}\n"},
		map[string]string{"core/a.rs": "// new contract\npub fn f() {}\n"})
	cl := lineAt(t, st.Layers[0], SideNew, "// new contract")
	if cl.Class != ClassLive || cl.Floor != TierGlance {
		t.Fatalf("class %s floor %s, want live at Glance", cl.Class, cl.Floor)
	}
}

func Test_tripwire_floors_an_unprotected_line(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": "unsafe fn f() {}\nfn g() {}\n"})
	d := st.Layers[0]
	if cl := lineAt(t, d, SideNew, "unsafe fn f() {}"); cl.Floor != TierGlance || !hasFlag(cl, "tripwire:unsafe") {
		t.Errorf("unsafe line: floor %s flags %v", cl.Floor, cl.Flags)
	}
	if cl := lineAt(t, d, SideNew, "fn g() {}"); cl.Floor != TierNone {
		t.Errorf("plain line: floor %s, want none", cl.Floor)
	}
}

func Test_generated_marker_and_lockfile_pins(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"Cargo.lock": "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n"},
		map[string]string{
			"Cargo.lock": "[[package]]\nname = \"a\"\nversion = \"1.1.0\"\n",
			"src/gen.rs": "// @generated by a tool\npub fn g() {}\n",
		})
	d := st.Layers[0]
	if c := classOfText(t, d, SideNew, "pub fn g() {}"); c != ClassGenerated {
		t.Errorf("generated file: %s", c)
	}
	if f := d.File("Cargo.lock", SideNew); f.PinsMoved != 1 {
		t.Errorf("Cargo.lock pins moved: %d, want 1", f.PinsMoved)
	}
}

func Test_every_line_numstat_matches(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/a.rs": block("a", 10), "doc.md": "x\n"},
		map[string]string{"src/a.rs": block("b", 12), "doc.md": "y\n", "x/tests/t.rs": "fn t() {}\n"})
	d := st.Layers[0]
	if len(d.Lines()) != d.Numstat {
		t.Fatalf("%d lines, numstat %d", len(d.Lines()), d.Numstat)
	}
}

func Test_budget_formula_boundaries(t *testing.T) {
	for live, want := range map[int]Budget{
		0:    {150, 5},
		1494: {150, 5},     // 149.4 rounds to 149: the 150 minimum
		1505: {151, 4 + 1}, // 150.5 rounds to 151; 151/40 is under 5
		1643: {164, 5},
		3034: {303, 8},
		5809: {581, 15},
		8000: {800, 20}, // exactly 40 per card
		8010: {801, 21}, // one line over: one more card
	} {
		if got := BudgetFor(live); got != want {
			t.Errorf("BudgetFor(%d) = %+v, want %+v", live, got, want)
		}
	}
}
