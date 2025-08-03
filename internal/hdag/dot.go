package hdag

import (
	"fmt"
	"io"
)

type dotConfig[V any] struct {
	vertexRenderer func(V) string
	nodeExtra      func(V) string
}

type DotOption[V any] func(*dotConfig[V])

func WithVertexRenderer[V any](f func(V) string) DotOption[V] {
	return func(p *dotConfig[V]) {
		p.vertexRenderer = f
	}
}

func WithNodeExtra[V any](f func(V) string) DotOption[V] {
	return func(p *dotConfig[V]) {
		p.nodeExtra = f
	}
}

func Dot[V any](w io.Writer, dag *DAG[V], options ...DotOption[V]) error {
	cfg := &dotConfig[V]{
		vertexRenderer: func(v V) string {
			return fmt.Sprint(v)
		},
		nodeExtra: func(v V) string {
			return ""
		},
	}
	for _, option := range options {
		option(cfg)
	}

	_, err := fmt.Fprintf(w, `
digraph G  {
    fontname="Helvetica,Arial,sans-serif"
    node [fontname="Helvetica,Arial,sans-serif"]
    edge [fontname="Helvetica,Arial,sans-serif"]
    node [fontsize=10, shape=box, height=0.25]
    edge [fontsize=10]

`)
	if err != nil {
		return err
	}

	for targetid, target := range dag.GetVertices() {
		extra := cfg.nodeExtra(target)
		if extra != "" {
			extra = "," + extra
		}
		_, err = fmt.Fprintf(w, "    %q [label=%q%s];\n", targetid, cfg.vertexRenderer(target), extra)
		if err != nil {
			return err
		}
	}

	_, err = fmt.Fprintf(w, "\n")
	if err != nil {
		return err
	}

	for targetid, target := range dag.GetVertices() {
		for childrenId := range dag.GetChildren(target) {
			_, err = fmt.Fprintf(w, "    %q -> %q;\n", targetid, childrenId)
			if err != nil {
				return err
			}
		}
	}

	_, err = fmt.Fprintf(w, "}\n")
	if err != nil {
		return err
	}

	return nil
}
