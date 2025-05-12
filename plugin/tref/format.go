package tref

import (
	"fmt"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lfu"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"strings"
	"sync"
)

type Ref = pluginv1.TargetRef
type RefOut = pluginv1.TargetRefWithOutput

var _ Refable = (*Ref)(nil)
var _ Refable = (*RefOut)(nil)
var _ RefableOut = (*RefOut)(nil)

type Refable interface {
	GetArgs() map[string]string
	GetPackage() string
	GetName() string
}

type RefableOut interface {
	Refable
	GetOutput() string
	GetFilters() []string
}

var m = cache.New[uint64, func() string](cache.AsLFU[uint64, func() string](lfu.WithCapacity(10000)))

func FormatFile(pkg string, file string) string {
	return Format(&pluginv1.TargetRef{
		Package: JoinPackage("@heph/file", pkg),
		Name:    "content",
		Args:    map[string]string{"f": file},
	})
}

func Format(ref Refable) string {
	if refh, ok := ref.(hproto.Hashable); ok {
		h := xxh3.New()
		refh.HashPB(h, nil)

		sum := h.Sum64()

		f, _ := m.GetOrSet(sum, sync.OnceValue(func() string { return format(ref) }))

		return f()
	}

	return format(ref)
}

func format(ref Refable) string {
	var sb strings.Builder
	sb.WriteString("//")
	sb.WriteString(ref.GetPackage())
	sb.WriteString(":")
	sb.WriteString(ref.GetName())

	if len(ref.GetArgs()) > 0 {
		sb.WriteString("@")
		first := true
		for k, v := range hmaps.Sorted(ref.GetArgs()) {
			if !first {
				sb.WriteString(",")
			} else {
				first = false
			}

			sb.WriteString(k)
			sb.WriteString("=")
			if strings.ContainsAny(v, ` ,"'`) {
				sb.WriteString(fmt.Sprintf("%q", v))
			} else {
				sb.WriteString(v)
			}
		}
	}

	if ref, ok := ref.(RefableOut); ok {
		out := ref.GetOutput()
		if out != "" {
			sb.WriteString("|")
			sb.WriteString(out)
		}

		if len(ref.GetFilters()) > 0 {
			sb.WriteString(" filters=")
			first := true
			for _, f := range ref.GetFilters() {
				if !first {
					sb.WriteString(",")
				} else {
					first = false
				}
				sb.WriteString(f)
			}
		}
	}

	return sb.String()
}
