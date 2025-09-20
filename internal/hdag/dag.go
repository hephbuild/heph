package hdag

import (
	"errors"
	"fmt"
	"iter"
	"slices"
	"sync"

	"github.com/heimdalr/dag"
)

type edgeMetaKey struct {
	src, dst string
}

type DAG[V any, M comparable] struct {
	d    *dag.DAG
	hash func(V) string

	mu       sync.Mutex
	edgeMeta map[edgeMetaKey][]M
}

func NewMeta[V any, M comparable](fn func(V) string) *DAG[V, M] {
	d := dag.NewDAG()
	d.Options(dag.Options{
		VertexHashFunc: func(v any) any {
			if v == nil {
				return nil
			}

			return fn(v.(V)) //nolint:errcheck
		},
	})

	return &DAG[V, M]{
		d:        d,
		hash:     fn,
		edgeMeta: map[edgeMetaKey][]M{},
	}
}

func New[V any](fn func(V) string) *DAG[V, struct{}] {
	return NewMeta[V, struct{}](fn)
}

func (d *DAG[V, M]) AddVertex(v V) error {
	return d.d.AddVertexByID(d.hash(v), v)
}

func (d *DAG[V, M]) AddEdge(src, dst V) error {
	var meta M

	return d.AddEdgeMeta(src, dst, meta)
}

var ErrDuplicateEdgeMeta = errors.New("duplicate edge meta")

func (d *DAG[V, M]) AddEdgeMeta(src, dst V, meta M) error {
	srcId, dstId := d.hash(src), d.hash(dst)

	d.mu.Lock()
	defer d.mu.Unlock()

	metaKey := edgeMetaKey{src: srcId, dst: dstId}
	if slices.Contains(d.edgeMeta[metaKey], meta) {
		return ErrDuplicateEdgeMeta
	}

	err := d.d.AddEdge(srcId, dstId)
	if err != nil && !IsDuplicateEdgeError(err) {
		return fmt.Errorf("%v: %w", meta, err)
	}

	d.edgeMeta[metaKey] = append(d.edgeMeta[metaKey], meta)

	return nil
}

func (d *DAG[V, M]) GetEdgeMeta(src, dst V) []M {
	d.mu.Lock()
	defer d.mu.Unlock()

	return d.edgeMeta[edgeMetaKey{src: d.hash(src), dst: d.hash(dst)}]
}

func (d *DAG[V, M]) getVertex(id string) (V, error) {
	v, err := d.d.GetVertex(id)
	if err != nil {
		var zero V
		return zero, err
	}

	return v.(V), nil //nolint:errcheck
}

func (d *DAG[V, M]) vertexes(m map[string]any) iter.Seq2[string, V] {
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

func (d *DAG[V, M]) GetVertices() iter.Seq2[string, V] {
	return d.vertexes(d.d.GetVertices())
}

func (d *DAG[V, M]) GetParents(v V) iter.Seq2[string, V] {
	m, err := d.d.GetParents(d.hash(v))
	if err != nil { // only unsane id
		panic(err)
	}

	return d.vertexes(m)
}

func (d *DAG[V, M]) GetChildren(v V) iter.Seq2[string, V] {
	m, err := d.d.GetChildren(d.hash(v))
	if err != nil { // only unsane id
		panic(err)
	}

	return d.vertexes(m)
}

func (d *DAG[V, M]) GetAncestorsGraph(v V) (*DAG[V, M], error) {
	sd, _, err := d.d.GetAncestorsGraph(d.hash(v))
	if err != nil {
		return nil, err
	}

	return repopulate(d, sd), nil
}

func repopulate[V any, M comparable](src *DAG[V, M], idd *dag.DAG) *DAG[V, M] {
	rd := NewMeta[V, M](src.hash)
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

			metas := src.GetEdgeMeta(v, child)
			for _, meta := range metas {
				err = rd.AddEdgeMeta(v, child, meta)
				if err != nil && !IsDuplicateEdgeError(err) {
					panic(err)
				}
			}
		}
	}))

	return rd
}

func (d *DAG[V, M]) GetDescendantsGraph(v V) (*DAG[V, M], error) {
	sd, _, err := d.d.GetDescendantsGraph(d.hash(v))
	if err != nil {
		return nil, err
	}

	return repopulate(d, sd), nil
}

func (d *DAG[V, M]) GetGraph(v V) (*DAG[V, M], error) {
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

			metas := descd.GetEdgeMeta(desc, child)
			for _, meta := range metas {
				err = sd.AddEdgeMeta(desc, child, meta)
				if err != nil && !IsDuplicateEdgeError(err) {
					return err
				}
			}
		}

		return nil
	})
	if err != nil {
		return nil, err
	}

	return sd, nil
}

func (d *DAG[V, M]) BFSWalk(visitor func(V) error) error {
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

func (d *DAG[V, M]) IsEdge(src, dst V) bool {
	is, err := d.d.IsEdge(d.hash(src), d.hash(dst))
	if err != nil {
		panic(err)
	}

	return is
}

type funcVisitor func(dag.Vertexer)

func (f funcVisitor) Visit(v dag.Vertexer) {
	f(v)
}

func IsDuplicateVertexError(err error) bool {
	return errors.As(err, &dag.VertexDuplicateError{}) || errors.As(err, &dag.IDDuplicateError{})
}

func IsDuplicateEdgeError(err error) bool {
	return errors.As(err, &dag.EdgeDuplicateError{}) || errors.Is(err, ErrDuplicateEdgeMeta)
}
