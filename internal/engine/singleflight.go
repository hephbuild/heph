package engine

import (
	"context"
	"fmt"
	"slices"
	"sync"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"

	"github.com/hephbuild/heph/internal/hproto"
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
	res *ExecuteChResult
	chs []chan *ExecuteChResult
}

func (h *singleflightResultHandle) getRes() *ExecuteChResult {
	res := *h.res
	res.Artifacts = slices.Clone(res.Artifacts)

	for i, output := range res.Artifacts {
		res.Artifacts[i].Artifact = hproto.Clone(output.Artifact)
	}

	return &res
}

func (h *singleflightResultHandle) newCh() <-chan *ExecuteChResult {
	h.mu.Lock()
	defer h.mu.Unlock()

	ch := make(chan *ExecuteChResult, 1)
	if h.res != nil {
		ch <- h.getRes()
		return ch
	}

	h.chs = append(h.chs, ch)

	return ch
}

func (h *singleflightResultHandle) send(result *ExecuteChResult) {
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

func (s *singleflightResult) getHandle(ctx context.Context, ref *pluginv1.TargetRef, outputs []string) (*singleflightResultHandle, bool) {
	outputs = slices.Clone(outputs)
	slices.Sort(outputs)
	if len(outputs) == 0 {
		outputs = nil
	}
	key := fmt.Sprintf("%s %#v", tref.Format(ref), outputs)

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

func (s *Singleflight) Result(ctx context.Context, ref *pluginv1.TargetRef, outputs []string) (<-chan *ExecuteChResult, func(result *ExecuteChResult), bool) {
	h, isNew := s.result.getHandle(ctx, ref, outputs)

	return h.newCh(), h.send, isNew
}
