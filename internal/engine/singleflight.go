package engine

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"sync"
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

func (h *singleflightResultHandle) newCh() <-chan *ExecuteResult {
	h.mu.Lock()
	defer h.mu.Unlock()

	ch := make(chan *ExecuteResult, 1)
	if h.res != nil {
		ch <- h.res
		return ch
	}

	h.chs = append(h.chs, ch)

	return ch
}

func (h *singleflightResultHandle) send(result *ExecuteResult) {
	h.mu.Lock()
	defer h.mu.Unlock()

	h.res = result
	for _, ch := range h.chs {
		ch <- h.res
		close(ch)
	}

	h.chs = nil
}

func (s *singleflightResult) getHandle(ctx context.Context, pkg string, name string, outputs []string) (*singleflightResultHandle, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()

	outputs = slices.Clone(outputs)
	slices.Sort(outputs)
	key := fmt.Sprintf("%s %s %s", pkg, name, strings.Join(outputs, ":"))

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
