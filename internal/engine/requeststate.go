package engine

import (
	"context"
	"fmt"
	"maps"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"

	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hdag"
	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type DAG = hdag.DAG[*pluginv1.TargetRef, string]

type memProbeInnerKey struct {
	name, pkg string
}

type RequestStateData struct {
	InteractiveExec func(context.Context, InteractiveExecOptions) error
	Shell           *pluginv1.TargetRef
	Force           *pluginv1.TargetMatcher
	Interactive     *pluginv1.TargetRef

	memSpec       hsingleflight.GroupMemContext[string, *pluginv1.TargetSpec]
	memProbe      hsingleflight.GroupMemContext[string, []*pluginv1.ProviderState]
	memProbeInner hsingleflight.GroupMemContext[memProbeInnerKey, []*pluginv1.ProviderState]
	memMeta       hsingleflight.GroupMemContext[string, *Meta]

	memLink hsingleflight.GroupMemContext[string, *LightLinkedTarget]
	memDef  hsingleflight.GroupMemContext[string, *TargetDef]

	memResult  hsingleflight.GroupMemContext[string, *ExecuteResultLocks]
	memExecute hsingleflight.GroupMemContext[string, *ExecuteResultLocks]

	dag *DAG
}

type RequestState struct {
	ID string

	*RequestStateData

	parent *pluginv1.TargetRef
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
	}
}

type Stack[K comparable] struct {
	m           map[K]*K
	first       *K
	last        *K
	debugString string
}

func NewStackRecursionError(printer func() string) *StackRecursionError {
	if false {
		buf := make([]byte, 4096)
		n := runtime.Stack(buf, false)

		stack := string(buf[:n]) + "\n"

		ogprinter := printer
		printer = func() string {
			return stack + ogprinter()
		}
	}

	return &StackRecursionError{printer: printer}
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
		return s, NewStackRecursionError(func() string {
			return s.StringWith(" -> ", k)
		})
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
