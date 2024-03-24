package worker2

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func ids(deps []Dep) []string {
	ids := make([]string, 0)
	for _, dep := range deps {
		ids = append(ids, dep.GetID())
	}

	return ids
}

func ids2(deps []*Deps) []string {
	ids := make([]string, 0)
	for _, dep := range deps {
		ids = append(ids, dep.id)
	}

	return ids
}

func TestTransitive(t *testing.T) {
	a1 := &Action{
		ID:   "1",
		Deps: NewDepsID("1"),
	}

	a2 := &Action{
		ID:   "2",
		Deps: NewDepsID("2", a1),
	}

	//a3 := &Action{
	//	ID:   "3",
	//	Deps: NewDeps(a2).WithID("3"),
	//}

	assert.EqualValues(t, []string{}, ids(a1.Deps.Dependencies()))
	assert.EqualValues(t, []string{}, ids(a1.Deps.TransitiveDependencies()))
	assert.EqualValues(t, []string{"2"}, ids2(a1.Deps.Dependees()))
	assert.EqualValues(t, []string{"2"}, ids2(a1.Deps.TransitiveDependees()))

	assert.EqualValues(t, []string{"1"}, ids(a2.Deps.Dependencies()))
	assert.EqualValues(t, []string{"1"}, ids(a2.Deps.TransitiveDependencies()))
	assert.EqualValues(t, []string{}, ids2(a2.Deps.Dependees()))
	assert.EqualValues(t, []string{}, ids2(a2.Deps.TransitiveDependees()))

	//assert.EqualValues(t, []string{"2"}, ids(a3.Deps.Get()))
	//assert.EqualValues(t, []string{"1", "2"}, ids(a3.Deps.GetTransitive()))

	a0 := &Action{
		ID:   "0",
		Deps: NewDepsID("0"),
	}

	fmt.Println("########### add 0 to 1")
	a1.Deps.Add(a0)

	assert.EqualValues(t, []string{}, ids(a0.Deps.Dependencies()))
	assert.EqualValues(t, []string{}, ids(a0.Deps.TransitiveDependencies()))
	assert.EqualValues(t, []string{"1"}, ids2(a0.Deps.Dependees()))
	assert.EqualValues(t, []string{"2", "1"}, ids2(a0.Deps.TransitiveDependees()))

	assert.EqualValues(t, []string{"0"}, ids(a1.Deps.Dependencies()))
	assert.EqualValues(t, []string{"0"}, ids(a1.Deps.TransitiveDependencies()))
	assert.EqualValues(t, []string{"2"}, ids2(a1.Deps.Dependees()))
	assert.EqualValues(t, []string{"2"}, ids2(a1.Deps.TransitiveDependees()))

	assert.EqualValues(t, []string{"1"}, ids(a2.Deps.Dependencies()))
	assert.EqualValues(t, []string{"1", "0"}, ids(a2.Deps.TransitiveDependencies()))
	assert.EqualValues(t, []string{}, ids2(a2.Deps.Dependees()))
	assert.EqualValues(t, []string{}, ids2(a2.Deps.TransitiveDependees()))

	//assert.EqualValues(t, []string{"2"}, ids(a3.Deps.Get()))
	//assert.EqualValues(t, []string{"1", "2", "0"}, ids(a3.Deps.GetTransitive()))
}
