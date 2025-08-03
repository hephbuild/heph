package hdag

import (
	"errors"
	"iter"

	"github.com/heimdalr/dag"
)

type DAG[V any] struct {
	d    *dag.DAG
	hash func(V) string
}

func New[V any](fn func(V) string) *DAG[V] {
	d := dag.NewDAG()
	d.Options(dag.Options{
		VertexHashFunc: func(v any) any {
			if v == nil {
				return nil
			}

			return fn(v.(V)) //nolint:errcheck
		},
	})

	return &DAG[V]{
		d:    d,
		hash: fn,
	}
}

func (d *DAG[V]) AddVertex(v V) error {
	return d.d.AddVertexByID(d.hash(v), v)
}

func (d *DAG[V]) AddEdge(src, dst V) error {
	return d.d.AddEdge(d.hash(src), d.hash(dst))
}

func (d *DAG[V]) getVertex(id string) (V, error) {
	v, err := d.d.GetVertex(id)
	if err != nil {
		var zero V
		return zero, err
	}

	return v.(V), nil //nolint:errcheck
}

func (d *DAG[V]) vertexes(m map[string]any) iter.Seq2[string, V] {
	return func(yield func(string, V) bool) {
		for id := range m {
			v, err := d.getVertex(id)
			if err != nil {
				panic(err)
			}

			if !yield(id, v) {
				return
			}
		}
	}
}

func (d *DAG[V]) GetVertices() iter.Seq2[string, V] {
	return d.vertexes(d.d.GetVertices())
}

func (d *DAG[V]) GetParents(v V) iter.Seq2[string, V] {
	m, err := d.d.GetParents(d.hash(v))
	if err != nil { // only unsane id
		panic(err)
	}

	return d.vertexes(m)
}

func (d *DAG[V]) GetChildren(v V) iter.Seq2[string, V] {
	m, err := d.d.GetChildren(d.hash(v))
	if err != nil { // only unsane id
		panic(err)
	}

	return d.vertexes(m)
}

func (d *DAG[V]) GetAncestorsGraph(v V) (*DAG[V], error) {
	sd, _, err := d.d.GetAncestorsGraph(d.hash(v))
	if err != nil {
		return nil, err
	}

	return repopulate(d, sd), nil
}

func repopulate[V any](src *DAG[V], idd *dag.DAG) *DAG[V] {
	rd := New[V](src.hash)
	idd.BFSWalk(funcVisitor(func(vertexer dag.Vertexer) {
		id, srcIda := vertexer.Vertex()
		srcId := srcIda.(string) //nolint:errcheck

		v, err := src.getVertex(srcId)
		if err != nil {
			panic(err)
		}

		err = rd.AddVertex(v)
		if err != nil && !IsDuplicateVertexError(err) {
			panic(err)
		}

		children, err := idd.GetChildren(id)
		if err != nil {
			panic(err)
		}

		for _, childSrcIda := range children {
			childSrcId := childSrcIda.(string) //nolint:errcheck

			child, err := src.getVertex(childSrcId)
			if err != nil {
				panic(err)
			}

			err = rd.AddVertex(child)
			if err != nil && !IsDuplicateVertexError(err) {
				panic(err)
			}

			err = rd.AddEdge(v, child)
			if err != nil && !IsDuplicateEdgeError(err) {
				panic(err)
			}
		}
	}))

	return rd
}

func (d *DAG[V]) GetDescendantsGraph(v V) (*DAG[V], error) {
	sd, _, err := d.d.GetDescendantsGraph(d.hash(v))
	if err != nil {
		return nil, err
	}

	return repopulate(d, sd), nil
}

func (d *DAG[V]) GetGraph(v V) (*DAG[V], error) {
	sd, err := d.GetAncestorsGraph(v)
	if err != nil {
		return nil, err
	}

	descd, err := d.GetDescendantsGraph(v)
	if err != nil {
		return nil, err
	}

	err = descd.BFSWalk(func(desc V) error {
		err := sd.AddVertex(desc)
		if err != nil && !IsDuplicateVertexError(err) {
			return err
		}

		for _, child := range descd.GetChildren(desc) {
			err = sd.AddVertex(child)
			if err != nil && !IsDuplicateVertexError(err) {
				return err
			}

			err = sd.AddEdge(desc, child)
			if err != nil && !IsDuplicateEdgeError(err) {
				return err
			}
		}

		return nil
	})
	if err != nil {
		return nil, err
	}

	return sd, nil
}

func (d *DAG[V]) BFSWalk(visitor func(V) error) error {
	var err error
	d.d.BFSWalk(funcVisitor(func(vertexer dag.Vertexer) {
		if err != nil {
			return
		}

		id, _ := vertexer.Vertex()
		var v V
		v, err = d.getVertex(id)
		if err != nil {
			return
		}
		err = visitor(v)
	}))

	return err
}

type funcVisitor func(dag.Vertexer)

func (f funcVisitor) Visit(v dag.Vertexer) {
	f(v)
}

func IsDuplicateVertexError(err error) bool {
	return errors.As(err, &dag.VertexDuplicateError{}) || errors.As(err, &dag.IDDuplicateError{})
}

func IsDuplicateEdgeError(err error) bool {
	return errors.As(err, &dag.EdgeDuplicateError{})
}
