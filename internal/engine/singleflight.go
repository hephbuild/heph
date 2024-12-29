package engine

import (
	"context"
	"fmt"
	"slices"
	"sync"

	"github.com/hephbuild/hephv2/internal/hproto"
)

type Singleflight struct {
	result singleflightResult
}

type singleflightResult struct {
	mu sync.Mutex
	m  map[string]*singleflightResultHandle
}

type singleflightResultHandle struct {
	mu  sync.Mutex
	res *ExecuteResult
	chs []chan *ExecuteResult
}

func (h *singleflightResultHandle) getRes() *ExecuteResult {
	res := *h.res
	res.Outputs = slices.Clone(res.Outputs)

	for i, output := range res.Outputs {
		res.Outputs[i].Artifact = hproto.Clone(output.Artifact)
	}

	return &res
}

func (h *singleflightResultHandle) newCh() <-chan *ExecuteResult {
	h.mu.Lock()
	defer h.mu.Unlock()

	ch := make(chan *ExecuteResult, 1)
	if h.res != nil {
		ch <- h.getRes()
		return ch
	}

	h.chs = append(h.chs, ch)

	return ch
}

func (h *singleflightResultHandle) send(result *ExecuteResult) {
	h.mu.Lock()
	defer h.mu.Unlock()

	h.res = result
	h.res = h.getRes() // make a copy

	for _, ch := range h.chs {
		ch <- h.getRes()
		close(ch)
	}

	h.chs = nil
}

func (s *singleflightResult) getHandle(ctx context.Context, pkg string, name string, outputs []string) (*singleflightResultHandle, bool) {
	outputs = slices.Clone(outputs)
	slices.Sort(outputs)
	if len(outputs) == 0 {
		outputs = nil
	}
	key := fmt.Sprintf("%s %s %#v", pkg, name, outputs)

	s.mu.Lock()
	defer s.mu.Unlock()

	if s.m == nil {
		s.m = make(map[string]*singleflightResultHandle)
	}

	isNew := false
	h, ok := s.m[key]
	if !ok {
		isNew = true
		h = &singleflightResultHandle{}
	}
	s.m[key] = h

	return h, isNew
}

func (s *Singleflight) Result(ctx context.Context, pkg string, name string, outputs []string) (<-chan *ExecuteResult, func(result *ExecuteResult), bool) {
	h, isNew := s.result.getHandle(ctx, pkg, name, outputs)

	return h.newCh(), h.send, isNew
}
