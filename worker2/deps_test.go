package worker2

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"strings"
	"testing"
)

func s(s string) string {
	return strings.TrimSpace(s) + "\n"
}

func TestLink(t *testing.T) {
	d1 := &Action{name: "1", deps: NewDeps()}
	d2 := &Action{name: "2", deps: NewDeps(d1)}

	d3 := &Action{name: "3", deps: NewDeps()}
	d4 := &Action{name: "4", deps: NewDeps(d3)}

	for _, d := range []*Action{d1, d2, d3, d4} {
		d.LinkDeps()
	}

	assertDetached := func() {
		assert.Equal(t, s(`
1:
  deps: []
  tdeps: []
  depdees: [2]
  tdepdees: [2]
`), d1.GetDepsObj().DebugString())

		assert.Equal(t, s(`
2:
  deps: [1]
  tdeps: [1]
  depdees: []
  tdepdees: []
`), d2.GetDepsObj().DebugString())

		assert.Equal(t, s(`
3:
  deps: []
  tdeps: []
  depdees: [4]
  tdepdees: [4]
`), d3.GetDepsObj().DebugString())

		assert.Equal(t, s(`
4:
  deps: [3]
  tdeps: [3]
  depdees: []
  tdepdees: []
`), d4.GetDepsObj().DebugString())
	}

	assertDetached()

	d3.AddDep(d2)

	assert.Equal(t, s(`
1:
  deps: []
  tdeps: []
  depdees: [2]
  tdepdees: [2 4 3]
`), d1.GetDepsObj().DebugString())

	assert.Equal(t, s(`
2:
  deps: [1]
  tdeps: [1]
  depdees: [3]
  tdepdees: [4 3]
`), d2.GetDepsObj().DebugString())

	assert.Equal(t, s(`
3:
  deps: [2]
  tdeps: [2 1]
  depdees: [4]
  tdepdees: [4]
`), d3.GetDepsObj().DebugString())

	assert.Equal(t, s(`
4:
  deps: [3]
  tdeps: [3 2 1]
  depdees: []
  tdepdees: []
`), d4.GetDepsObj().DebugString())

	d3.GetDepsObj().Remove(d2)

	assertDetached()
}

func TestCycle1(t *testing.T) {
	d1 := &Action{name: "1", deps: NewDeps()}
	d2 := &Action{name: "2", deps: NewDeps(d1)}
	d3 := &Action{name: "3", deps: NewDeps(d2)}

	assert.PanicsWithValue(t, "cycle", func() {
		d2.AddDep(d3)
	})
}

func TestCycle2(t *testing.T) {
	d1 := &Action{name: "1", deps: NewDeps( /* d4 */ )}
	d2 := &Action{name: "2", deps: NewDeps(d1)}

	d3 := &Action{name: "3", deps: NewDeps()}
	d4 := &Action{name: "4", deps: NewDeps(d3)}

	d1.AddDep(d4)

	assert.PanicsWithValue(t, "cycle", func() {
		d3.AddDep(d2)
	})
}

func TestRemoveStress(t *testing.T) {
	root := &Action{name: "root", deps: NewDeps()}
	root.LinkDeps()

	for i := 0; i < 1000; i++ {
		d := &Action{name: fmt.Sprint(i)}
		d.LinkDeps()
		root.AddDep(d)

		for j := 0; j < 1000; j++ {
			d1 := &Action{name: fmt.Sprintf("%v-%v", i, j)}
			d1.LinkDeps()
			d.AddDep(d1)
		}
	}

	group := &Action{name: "group", deps: NewDeps(root)}
	group.LinkDeps()

	group.GetDepsObj().Remove(root)
}
