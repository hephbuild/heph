package graphdot

import (
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/sets"
	"strconv"
	"time"
)

func Print(dag *graph.DAG, tools bool) {
	fmt.Printf(`
digraph G  {
	fontname="Helvetica,Arial,sans-serif"
	node [fontname="Helvetica,Arial,sans-serif"]
	edge [fontname="Helvetica,Arial,sans-serif"]
	rankdir="LR"
	node [fontsize=10, shape=box, height=0.25]
	edge [fontsize=10]
`)
	id := func(target *graph.Target) string {
		return strconv.Quote(target.Addr)
	}

	for _, target := range dag.GetVertices() {
		extra := ""
		if target.IsGroup() {
			//extra = ` color="red"`
		}

		log.Tracef("walk %v", target.Addr)

		parentsStart := time.Now()
		parents, err := dag.GetParents(target)
		log.Debugf("parents took %v (got %v)", time.Since(parentsStart), len(parents))
		if err != nil {
			panic(err)
		}

		fmt.Printf("    %v [label=\"%v\"%v];\n", id(target), target.Addr, extra)

		skip := sets.NewStringSet(0)
		if !tools {
			for _, tool := range target.Tools.Targets {
				skip.Add(tool.Target.Addr)
			}
		}

		for _, ancestor := range parents {
			if skip.Has(ancestor.Addr) {
				continue
			}

			fmt.Printf("    %v -> %v;\n", id(ancestor), id(target))
		}
		fmt.Println()
	}

	fmt.Println("}")
}
