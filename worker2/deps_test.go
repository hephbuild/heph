package worker2

import (
	"github.com/stretchr/testify/assert"
	"strings"
	"testing"
)

func s(s string) string {
	return strings.TrimSpace(s) + "\n"
}

func TestLink(t *testing.T) {
	d1 := &Action{ID: "1", Deps: NewDeps()}
	d2 := &Action{ID: "2", Deps: NewDeps(d1)}

	d3 := &Action{ID: "3", Deps: NewDeps()}
	d4 := &Action{ID: "4", Deps: NewDeps(d3)}

	d3.AddDep(d2)

	d4.LinkDeps()

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
}
