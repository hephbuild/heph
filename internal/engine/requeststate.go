package engine

import (
	"context"
	"fmt"
	"maps"
	"os"
	"strconv"
	"strings"
	"sync"

	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hdag"
	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type memSpecGetKey struct {
	providerName, refKey string
}

type DAG = hdag.DAG[*pluginv1.TargetRef, string]

type RequestStateData struct {
	InteractiveExec func(context.Context, InteractiveExecOptions) error
	Shell           *pluginv1.TargetRef
	Force           *pluginv1.TargetMatcher
	Interactive     *pluginv1.TargetRef

	memSpec hsingleflight.GroupMemContext[string, *pluginv1.TargetSpec]
	// memProbe   hsingleflight.GroupMemContext[string, []*pluginv1.ProviderState]
	memMeta hsingleflight.GroupMemContext[string, *Meta]

	memLink hsingleflight.GroupMemContext[string, *LightLinkedTarget]
	memDef  hsingleflight.GroupMemContext[string, *TargetDef]

	memResult  hsingleflight.GroupMemContext[string, *ExecuteResultLocks]
	memExecute hsingleflight.GroupMemContext[string, *ExecuteResultLocks]

	dag *DAG
}

type traceStackEntry struct {
	fun string
	id1 string
	id2 string
}

type RequestState struct {
	ID string

	*RequestStateData

	traceStack Stack[traceStackEntry]
	parent     *pluginv1.TargetRef
}

func (s *RequestState) Trace(fun, id string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: fun, id1: id})
}

func (s *RequestState) HasTrace(fun, id string) bool {
	return s.traceStack.Has(traceStackEntry{fun: fun, id1: id})
}

func (s *RequestState) TraceList(name string, pkg string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "List", id1: name, id2: pkg})
}

func (s *RequestState) TraceProviderCall(name string, pkg string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "ProviderCall", id1: name, id2: pkg})
}

func (s *RequestState) TraceResolveProvider(format string, name string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "ResolveProvider", id1: format, id2: name})
}

func (s *RequestState) TraceQueryListProvider(format string, name string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "QueryListProvider", id1: format, id2: name})
}

func (s *RequestState) traceStackPush(e traceStackEntry) (*RequestState, error) {
	stack, err := s.traceStack.Push(e)
	if err != nil {
		return nil, err
	}

	return &RequestState{
		ID:               uuid.New().String(),
		RequestStateData: s.RequestStateData,
		traceStack:       stack,
	}, nil
}

func (s *RequestState) WithParent(parent *pluginv1.TargetRef) *RequestState {
	rs := s.Copy()
	rs.parent = parent

	return rs
}

func (s *RequestState) Copy() *RequestState {
	return &RequestState{
		ID:               uuid.New().String(),
		RequestStateData: s.RequestStateData,
		traceStack:       s.traceStack,
	}
}

type Stack[K comparable] struct {
	m           map[K]*K
	first       *K
	last        *K
	debugString string
}

type StackRecursionError struct {
	printer func() string
}

func (e StackRecursionError) Error() string {
	return fmt.Sprintf("stack recursion detected: %v", e.printer())
}

func (e StackRecursionError) Print() string {
	return e.printer()
}

func (e StackRecursionError) Is(err error) bool {
	_, ok := err.(StackRecursionError)

	return ok
}

func (s Stack[K]) Has(k K) bool {
	_, ok := s.m[k]

	return ok
}

var enableStackDebug = sync.OnceValue(func() bool {
	v, _ := strconv.ParseBool(os.Getenv("HEPH_DEBUG_STACK"))

	return v
})

func (s Stack[K]) Push(k K) (Stack[K], error) {
	if _, ok := s.m[k]; ok {
		return s, StackRecursionError{
			printer: func() string {
				return s.StringWith(" -> ", k)
			},
		}
	}

	s.m = maps.Clone(s.m)
	if s.m == nil {
		s.m = map[K]*K{}
		s.first = &k
	}
	if s.last != nil {
		s.m[*s.last] = &k
	}
	s.m[k] = nil
	s.last = &k

	if enableStackDebug() {
		s.debugString = s.StringWith("\n")
	}

	return s, nil
}

func (s Stack[K]) String() string {
	return s.StringWith(" -> ")
}

func (s Stack[K]) StringWith(sep string, v ...K) string {
	var buf strings.Builder
	next := s.first
	for next != nil {
		current := *next

		if buf.Len() > 0 {
			buf.WriteString(sep)
		}

		fmt.Fprintf(&buf, "%v", current)

		next = s.m[current]
	}

	if len(v) > 0 {
		buf.WriteString(sep)
		fmt.Fprintf(&buf, "%v", v[0])
	}

	return buf.String()
}
