package hdag

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"slices"
	"sync"

	"github.com/hephbuild/heph/internal/hsync"
)

var (
	ErrDuplicateEdgeMeta = errors.New("duplicate edge meta")
)

type VertexNotFoundError struct {
	ID string
}

func (e VertexNotFoundError) Error() string { return fmt.Sprintf("vertex %s not found", e.ID) }

type EdgeMetaError struct {
	Meta any
	Err  error
}

func (e EdgeMetaError) Error() string { return fmt.Sprintf("%v: %v", e.Meta, e.Err) }
func (e EdgeMetaError) Unwrap() error { return e.Err }

type SelfLoopError struct {
	Src string
	Dst string
}

func (e SelfLoopError) Error() string {
	return fmt.Sprintf("self loop not allowed for vertex %s", e.Src)
}

type CycleError struct {
	Src string
	Dst string
}

func (e CycleError) Error() string {
	return fmt.Sprintf("edge %s -> %s would create a cycle", e.Src, e.Dst)
}

type VertexDuplicateError struct{}

func (e VertexDuplicateError) Error() string { return "vertex already exists" }

type EdgeDuplicateError struct{}

func (e EdgeDuplicateError) Error() string { return "edge already exists" }

func IsDuplicateVertexError(err error) bool {
	var e VertexDuplicateError
	return errors.As(err, &e)
}

func IsDuplicateEdgeError(err error) bool {
	var e EdgeDuplicateError
	return errors.As(err, &e) || errors.Is(err, ErrDuplicateEdgeMeta)
}

func IsVertexNotFoundError(err error) bool {
	var e VertexNotFoundError
	return errors.As(err, &e)
}

type edgeValue[M comparable] struct {
	mu    sync.RWMutex
	metas []M
}

type node[V any, M comparable] struct {
	v        V
	id       string
	mu       sync.RWMutex
	parents  map[string]*edgeValue[M]
	children map[string]*edgeValue[M]
}

type DAG[V any, M comparable] struct {
	hash     func(V) string
	vertices hsync.Map[string, *node[V, M]]

	edgeAddMu sync.Mutex
}

func NewMeta[V any, M comparable](fn func(V) string) *DAG[V, M] {
	return &DAG[V, M]{
		hash: fn,
	}
}

func New[V any](fn func(V) string) *DAG[V, struct{}] {
	return NewMeta[V, struct{}](fn)
}

func (d *DAG[V, M]) getNode(id string) *node[V, M] {
	v, ok := d.vertices.Load(id)
	if !ok {
		return nil
	}
	return v
}

func (d *DAG[V, M]) AddVertex(v V) error {
	id := d.hash(v)
	_, loaded := d.vertices.LoadOrStore(id, &node[V, M]{
		id:       id,
		v:        v,
		parents:  make(map[string]*edgeValue[M]),
		children: make(map[string]*edgeValue[M]),
	})
	if loaded {
		return VertexDuplicateError{}
	}
	return nil
}

func (d *DAG[V, M]) rootAddEdge(srcId, dstId string) (*edgeValue[M], error) {
	if srcId == dstId {
		return nil, SelfLoopError{Src: srcId, Dst: dstId}
	}

	srcNode := d.getNode(srcId)
	if srcNode == nil {
		return nil, VertexNotFoundError{ID: srcId}
	}
	dstNode := d.getNode(dstId)
	if dstNode == nil {
		return nil, VertexNotFoundError{ID: dstId}
	}

	srcNode.mu.RLock()
	edge, exists := srcNode.children[dstId]
	srcNode.mu.RUnlock()
	if exists {
		return edge, EdgeDuplicateError{}
	}

	d.edgeAddMu.Lock()
	defer d.edgeAddMu.Unlock()

	srcNode.mu.RLock()
	edge, exists = srcNode.children[dstId]
	srcNode.mu.RUnlock()
	if exists {
		return edge, EdgeDuplicateError{}
	}

	if d.reaches(dstNode, srcId) {
		return nil, CycleError{Src: srcId, Dst: dstId}
	}

	edge = &edgeValue[M]{}

	srcNode.mu.Lock()
	srcNode.children[dstId] = edge
	srcNode.mu.Unlock()

	dstNode.mu.Lock()
	dstNode.parents[srcId] = edge
	dstNode.mu.Unlock()

	return edge, nil
}

func (d *DAG[V, M]) reaches(start *node[V, M], targetId string) bool {
	visited := make(map[string]bool)
	queue := []string{start.id}
	visited[start.id] = true

	for len(queue) > 0 {
		currId := queue[0]
		queue = queue[1:]

		if currId == targetId {
			return true
		}

		currNode := d.getNode(currId)
		if currNode == nil {
			continue
		}

		currNode.mu.RLock()
		for childId := range currNode.children {
			if !visited[childId] {
				visited[childId] = true
				queue = append(queue, childId)
			}
		}
		currNode.mu.RUnlock()
	}
	return false
}

func (d *DAG[V, M]) AddEdge(src, dst V) error {
	var meta M
	return d.AddEdgeMeta(src, dst, meta)
}

func (d *DAG[V, M]) AddEdgeMeta(src, dst V, meta M) error {
	srcId := d.hash(src)
	dstId := d.hash(dst)

	edge, err := d.rootAddEdge(srcId, dstId)
	if err != nil {
		if !IsDuplicateEdgeError(err) {
			return EdgeMetaError{Meta: meta, Err: err}
		}
	}

	edge.mu.Lock()
	defer edge.mu.Unlock()

	if slices.Contains(edge.metas, meta) {
		return ErrDuplicateEdgeMeta
	}
	edge.metas = append(edge.metas, meta)

	return nil
}

func (d *DAG[V, M]) GetEdgeMeta(src, dst V) []M {
	srcId := d.hash(src)
	dstId := d.hash(dst)

	srcNode := d.getNode(srcId)
	if srcNode == nil {
		return nil
	}

	srcNode.mu.RLock()
	edge, ok := srcNode.children[dstId]
	srcNode.mu.RUnlock()

	if !ok {
		return nil
	}

	edge.mu.RLock()
	defer edge.mu.RUnlock()
	if len(edge.metas) == 0 {
		return nil
	}
	res := make([]M, len(edge.metas))
	copy(res, edge.metas)
	return res
}

func (d *DAG[V, M]) GetVertices() iter.Seq2[string, V] {
	return func(yield func(string, V) bool) {
		d.vertices.Range(func(key string, value *node[V, M]) bool {
			return yield(key, value.v)
		})
	}
}

func (d *DAG[V, M]) GetParents(v V) iter.Seq2[string, V] {
	return func(yield func(string, V) bool) {
		id := d.hash(v)
		n := d.getNode(id)
		if n == nil {
			return
		}

		n.mu.RLock()
		parents := make([]string, 0, len(n.parents))
		for pid := range n.parents {
			parents = append(parents, pid)
		}
		n.mu.RUnlock()

		for _, pid := range parents {
			pn := d.getNode(pid)
			if pn == nil {
				continue
			}
			if !yield(pid, pn.v) {
				return
			}
		}
	}
}

func (d *DAG[V, M]) GetChildren(v V) iter.Seq2[string, V] {
	return func(yield func(string, V) bool) {
		id := d.hash(v)
		n := d.getNode(id)
		if n == nil {
			return
		}

		n.mu.RLock()
		children := make([]string, 0, len(n.children))
		for cid := range n.children {
			children = append(children, cid)
		}
		n.mu.RUnlock()

		for _, cid := range children {
			cn := d.getNode(cid)
			if cn == nil {
				continue
			}
			if !yield(cid, cn.v) {
				return
			}
		}
	}
}

func (d *DAG[V, M]) GetAncestorsGraph(ctx context.Context, v V) (*DAG[V, M], error) {
	rd := NewMeta[V, M](d.hash)
	startId := d.hash(v)
	startNode := d.getNode(startId)
	if startNode == nil {
		return nil, VertexNotFoundError{ID: startId}
	}
	_ = rd.AddVertex(startNode.v)
	queue := []string{startId}
	visited := map[string]bool{startId: true}

	for len(queue) > 0 {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}

		currId := queue[0]
		queue = queue[1:]

		currNode := d.getNode(currId)
		if currNode == nil {
			continue
		}

		currNode.mu.RLock()
		parents := make([]string, 0, len(currNode.parents))
		for pid := range currNode.parents {
			parents = append(parents, pid)
		}
		currNode.mu.RUnlock()

		for _, pid := range parents {
			pNode := d.getNode(pid)
			if pNode == nil {
				continue
			}
			if !visited[pid] {
				visited[pid] = true
				queue = append(queue, pid)
			}
			_ = rd.AddVertex(pNode.v)

			metas := d.GetEdgeMeta(pNode.v, currNode.v)
			if len(metas) == 0 {
				_ = rd.AddEdge(pNode.v, currNode.v)
			} else {
				for _, m := range metas {
					_ = rd.AddEdgeMeta(pNode.v, currNode.v, m)
				}
			}
		}
	}
	return rd, nil
}

func (d *DAG[V, M]) GetDescendantsGraph(ctx context.Context, v V) (*DAG[V, M], error) {
	rd := NewMeta[V, M](d.hash)
	startId := d.hash(v)
	startNode := d.getNode(startId)
	if startNode == nil {
		return nil, VertexNotFoundError{ID: startId}
	}
	_ = rd.AddVertex(startNode.v)
	queue := []string{startId}
	visited := map[string]bool{startId: true}

	for len(queue) > 0 {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}

		currId := queue[0]
		queue = queue[1:]

		currNode := d.getNode(currId)
		if currNode == nil {
			continue
		}

		currNode.mu.RLock()
		children := make([]string, 0, len(currNode.children))
		for cid := range currNode.children {
			children = append(children, cid)
		}
		currNode.mu.RUnlock()

		for _, cid := range children {
			cNode := d.getNode(cid)
			if cNode == nil {
				continue
			}
			if !visited[cid] {
				visited[cid] = true
				queue = append(queue, cid)
			}
			_ = rd.AddVertex(cNode.v)

			metas := d.GetEdgeMeta(currNode.v, cNode.v)
			if len(metas) == 0 {
				_ = rd.AddEdge(currNode.v, cNode.v)
			} else {
				for _, m := range metas {
					_ = rd.AddEdgeMeta(currNode.v, cNode.v, m)
				}
			}
		}
	}
	return rd, nil
}

func (d *DAG[V, M]) GetGraph(ctx context.Context, v V) (*DAG[V, M], error) {
	sd, err := d.GetAncestorsGraph(ctx, v)
	if err != nil {
		return nil, err
	}

	descd, err := d.GetDescendantsGraph(ctx, v)
	if err != nil {
		return nil, err
	}

	for _, desc := range descd.GetVertices() {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}

		err := sd.AddVertex(desc)
		if err != nil && !IsDuplicateVertexError(err) {
			return nil, err
		}

		for _, child := range descd.GetChildren(desc) {
			err = sd.AddVertex(child)
			if err != nil && !IsDuplicateVertexError(err) {
				return nil, err
			}

			metas := descd.GetEdgeMeta(desc, child)
			if len(metas) == 0 {
				err = sd.AddEdge(desc, child)
				if err != nil && !IsDuplicateEdgeError(err) {
					return nil, err
				}
			} else {
				for _, meta := range metas {
					err = sd.AddEdgeMeta(desc, child, meta)
					if err != nil && !IsDuplicateEdgeError(err) {
						return nil, err
					}
				}
			}
		}
	}

	return sd, nil
}

func (d *DAG[V, M]) BFSWalk(ctx context.Context) iter.Seq[V] {
	return func(yield func(V) bool) {
		var roots []string

		d.vertices.Range(func(key string, n *node[V, M]) bool {
			n.mu.RLock()
			degree := len(n.parents)
			n.mu.RUnlock()
			if degree == 0 {
				roots = append(roots, n.id)
			}
			return true
		})

		slices.Sort(roots)

		visited := make(map[string]bool)
		queue := make([]string, len(roots))
		copy(queue, roots)
		for _, r := range roots {
			visited[r] = true
		}

		for len(queue) > 0 {
			if ctx.Err() != nil {
				return
			}
			currId := queue[0]
			queue = queue[1:]

			currNode := d.getNode(currId)
			if currNode == nil {
				continue
			}

			if !yield(currNode.v) {
				return
			}

			currNode.mu.RLock()
			children := make([]string, 0, len(currNode.children))
			for cid := range currNode.children {
				children = append(children, cid)
			}
			currNode.mu.RUnlock()

			slices.Sort(children)

			for _, cid := range children {
				if !visited[cid] {
					visited[cid] = true
					queue = append(queue, cid)
				}
			}
		}
	}
}

func (d *DAG[V, M]) IsEdge(src, dst V) bool {
	srcId := d.hash(src)
	dstId := d.hash(dst)

	srcNode := d.getNode(srcId)
	if srcNode == nil {
		return false
	}
	if d.getNode(dstId) == nil {
		return false
	}

	srcNode.mu.RLock()
	_, is := srcNode.children[dstId]
	srcNode.mu.RUnlock()

	return is
}
