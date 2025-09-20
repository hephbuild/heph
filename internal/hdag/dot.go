package hdag

import (
	"fmt"
	"io"
)

type dotConfigVertex[V any] struct {
	vertexRenderer func(V) string
	vertexFilter   func(V) bool
	vertexExtra    func(V) string
}
type dotConfigMeta[V any, M any] struct {
	edgeMetaRenderer func(src V, dst V, meta M) string
	edgeFilter       func(src V, dst V, meta M) bool
	edgeExtra        func(src V, dst V, meta M) string
}
type dotConfig[V any, M any] struct {
	dotConfigVertex[V]
	dotConfigMeta[V, M]
}

type DotOption[V, M any] interface {
	isOption()
}

type DotOptionVertex[V any] interface {
	isOption()
	ApplyVertex(p *dotConfigVertex[V])
}

type dotOptionVertexFunc[V any] func(p *dotConfigVertex[V])

func (d dotOptionVertexFunc[V]) isOption() {}

func (d dotOptionVertexFunc[V]) ApplyVertex(p *dotConfigVertex[V]) {
	d(p)
}

type DotOptionMeta[V, M any] interface {
	isOption()
	ApplyMeta(p *dotConfigMeta[V, M])
}

type dotOptionMetaFunc[V, M any] func(p *dotConfigMeta[V, M])

func (d dotOptionMetaFunc[V, M]) isOption() {}

func (d dotOptionMetaFunc[V, M]) ApplyMeta(p *dotConfigMeta[V, M]) {
	d(p)
}

func WithVertexRenderer[V any](f func(V) string) DotOptionVertex[V] {
	return dotOptionVertexFunc[V](func(p *dotConfigVertex[V]) {
		p.vertexRenderer = f
	})
}

func WithVertexExtra[V any](f func(V) string) DotOptionVertex[V] {
	return dotOptionVertexFunc[V](func(p *dotConfigVertex[V]) {
		p.vertexExtra = f
	})
}

func WithVertexFilter[V any](f func(V) bool) DotOptionVertex[V] {
	return dotOptionVertexFunc[V](func(p *dotConfigVertex[V]) {
		p.vertexFilter = f
	})
}

func WithEdgeRenderer[V, M any](f func(src V, dst V, meta M) string) DotOptionMeta[V, M] {
	return dotOptionMetaFunc[V, M](func(p *dotConfigMeta[V, M]) {
		p.edgeMetaRenderer = f
	})
}

func WithEdgeFilter[V, M any](f func(src V, dst V, meta M) bool) DotOptionMeta[V, M] {
	return dotOptionMetaFunc[V, M](func(p *dotConfigMeta[V, M]) {
		p.edgeFilter = f
	})
}

func Dot[V any, M comparable](w io.Writer, dag *DAG[V, M], options ...DotOption[V, M]) error {
	cfg := &dotConfig[V, M]{
		dotConfigVertex: dotConfigVertex[V]{
			vertexRenderer: func(v V) string {
				return fmt.Sprint(v)
			},
			vertexExtra: func(v V) string {
				return ""
			},
			vertexFilter: func(v V) bool {
				return true
			},
		},
		dotConfigMeta: dotConfigMeta[V, M]{
			edgeMetaRenderer: func(src V, dst V, meta M) string {
				return ""
			},
			edgeExtra: func(src V, dst V, meta M) string {
				return ""
			},
			edgeFilter: func(src V, dst V, meta M) bool {
				return true
			},
		},
	}
	for _, option := range options {
		switch option := option.(type) {
		case DotOptionVertex[V]:
			option.ApplyVertex(&cfg.dotConfigVertex)
		case DotOptionMeta[V, M]:
			option.ApplyMeta(&cfg.dotConfigMeta)
		default:
			panic(fmt.Sprintf("unknown option type: %T", options))
		}
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

	for targetid, v := range dag.GetVertices() {
		if !cfg.vertexFilter(v) {
			continue
		}

		extra := cfg.vertexExtra(v)
		if extra != "" {
			extra = "," + extra
		}
		_, err = fmt.Fprintf(w, "    %q [label=%q%s];\n", targetid, cfg.vertexRenderer(v), extra)
		if err != nil {
			return err
		}
	}

	_, err = fmt.Fprintf(w, "\n")
	if err != nil {
		return err
	}

	for targetid, v := range dag.GetVertices() {
		if !cfg.vertexFilter(v) {
			continue
		}

		for childrenId, child := range dag.GetChildren(v) {
			if !cfg.vertexFilter(child) {
				continue
			}

			metas := dag.GetEdgeMeta(v, child)

			for _, m := range metas {
				if !cfg.edgeFilter(v, child, m) {
					continue
				}

				extra := cfg.edgeExtra(v, child, m)
				if extra != "" {
					extra = "," + extra
				}
				_, err = fmt.Fprintf(w, "    %q -> %q [label=%q%s];\n", targetid, childrenId, cfg.edgeMetaRenderer(v, child, m), extra)
				if err != nil {
					return err
				}
			}
		}
	}

	_, err = fmt.Fprintf(w, "}\n")
	if err != nil {
		return err
	}

	return nil
}
