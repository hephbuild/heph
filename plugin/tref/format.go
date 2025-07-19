package tref

import (
	"fmt"
	"strings"
	"unsafe"

	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lfu"
	"github.com/hephbuild/heph/hsync"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
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

func FormatFile(pkg string, file string) string {
	return Format(&pluginv1.TargetRef{
		Package: JoinPackage("@heph/file", pkg),
		Name:    "content",
		Args:    map[string]string{"f": file},
	})
}

var formatCache = cache.New[uint64, string](cache.AsLFU[uint64, string](lfu.WithCapacity(10000)))
var formatSf = hsingleflight.Group[uint64, string]{}

var formatHashPool = hsync.Pool[*xxh3.Hasher]{New: xxh3.New}

func sumRef(ref hproto.Hashable) uint64 {
	h := formatHashPool.Get()
	defer formatHashPool.Put(h)
	h.Reset()

	switch ref := ref.(type) {
	case *pluginv1.TargetRef:
		sumRefTargetRef(h, ref)
	case *pluginv1.TargetRefWithOutput:
		sumRefTargetRefWithOutput(h, ref)
	default:
		ref.HashPB(h, nil)
	}

	return h.Sum64()
}

func sumRefTargetRef(hasher *xxh3.Hasher, m *pluginv1.TargetRef) {
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetPackage()), len(m.GetPackage())))
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetName()), len(m.GetName())))
	for k, v := range hmaps.Sorted(m.GetArgs()) {
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(k), len(k)))
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
	}
}

func sumRefTargetRefWithOutput(hasher *xxh3.Hasher, m *pluginv1.TargetRefWithOutput) {
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetPackage()), len(m.GetPackage())))
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetName()), len(m.GetName())))
	for k, v := range hmaps.Sorted(m.GetArgs()) {
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(k), len(k)))
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
	}
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetOutput()), len(m.GetOutput())))
	if len(m.GetFilters()) > 0 {
		for _, v := range m.GetFilters() {
			_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
		}
	}
}

func Format(ref Refable) string {
	if refh, ok := ref.(hproto.Hashable); ok {
		sum := sumRef(refh)

		f, ok := formatCache.Get(sum)
		if ok {
			return f
		}

		f, _, _ = formatSf.Do(sum, func() (string, error) {
			f, ok := formatCache.Get(sum)
			if ok {
				return f, nil
			}

			f = format(ref)

			formatCache.Set(sum, f)

			return f, nil
		})

		return f
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
