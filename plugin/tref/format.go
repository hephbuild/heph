package tref

import (
	"fmt"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lfu"
	"github.com/hephbuild/heph/hsync"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"hash"
	"iter"
	"strconv"
	"strings"
	"sync"
)

const inlinesArgs = 5

type Ref struct {
	pkg  string
	name string

	argsm map[string]string

	argskv  [inlinesArgs * 2]string
	argskvc uint16
}

func (r Ref) HashPB(hash hash.Hash, m map[string]struct{}) {
	hash.Write([]byte(r.GetPackage()))
	hash.Write([]byte(r.GetName()))

	hash.Write([]byte(strconv.Itoa(int(r.argskvc))))
	for k, v := range r.GetArgs() {
		hash.Write([]byte(k))
		hash.Write([]byte(v))
	}
}

func (r Ref) resetArgs() Ref {
	r.argskv = [inlinesArgs * 2]string{}
	r.argskvc = 0
	r.argsm = nil

	return r
}

func (r Ref) setArgs(m map[string]string) Ref {
	r = r.resetArgs()

	if len(m) < 5 {
		var i uint16
		for k, v := range hmaps.Sorted(m) {
			r.argskv[i] = k
			i++
			r.argskv[i] = v
			i++
		}
		r.argskvc = i
	} else {
		r.argsm = hmaps.From(hmaps.Sorted(m), len(m))
	}

	return r
}

func NewRef(pkg, name string, args map[string]string) Ref {
	r := Ref{
		pkg:  pkg,
		name: name,
	}

	r.setArgs(args)

	return r
}

func (r Ref) GetArgs() iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for i := uint16(0); i < r.argskvc; i++ {
			if !yield(r.argskv[i*2], r.argskv[i*2+1]) {
				return
			}
		}
	}
}

func (r Ref) GetArgsCount() int {
	return int(r.argskvc)
}

func (r Ref) GetArg(k string) (string, bool) {
	if r.argsm == nil {
		for i := uint16(0); i < r.argskvc; i++ {
			if r.argskv[i*2] == k {
				return r.argskv[i*2+1], true
			}
		}

		return "", false
	} else {
		v, ok := r.argsm[k]
		return v, ok
	}
}

func (r Ref) SetArg(k, v string) {
	if r.argsm == nil {
		for i := uint16(0); i < r.argskvc; i++ {
			if r.argskv[i*2] == k {
				r.argskv[i*2+1] = v
				return
			}
		}

		if r.argskvc < inlinesArgs {
			r.argskv[r.argskvc*2] = k
			r.argskv[r.argskvc*2+1] = v
			r.argskvc++
		} else {
			m := make(map[string]string, r.argskvc+1)
			for k, v := range r.GetArgs() {
				m[k] = v
			}
			m[k] = v
			r.resetArgs()

			r.argsm = m
			return
		}
	} else {
		r.argsm[k] = v
		return
	}
}

func (r Ref) GetPackage() string {
	return r.pkg
}

func (r Ref) GetName() string {
	return r.name
}

func (r Ref) ToProto() *pluginv1.TargetRef {
	return &pluginv1.TargetRef{
		Package: r.GetPackage(),
		Name:    r.GetName(),
		Args:    hmaps.From(r.GetArgs(), r.GetArgsCount()),
	}
}

func FromProto(ref *pluginv1.TargetRef) Ref {
	return NewRef(ref.GetPackage(), ref.GetName(), ref.GetArgs())
}

type RefWithOutput struct {
	Ref
	out     string
	filters []string
}

func (r RefWithOutput) GetOutput() *string {
	if r.out == "" {
		return nil
	}

	return &r.out
}

func (r RefWithOutput) GetFilters() []string {
	return r.filters
}

func NewRefOut(pkg, name string, args map[string]string, out string, filters []string) RefWithOutput {
	r := RefWithOutput{
		Ref:     NewRef(pkg, name, args),
		out:     out,
		filters: filters,
	}

	r.setArgs(args)

	return r
}

var _ Refable = Ref{}
var _ Refable = RefWithOutput{}
var _ RefableOut = RefWithOutput{}

type Refable interface {
	hproto.Hashable

	GetArgs() iter.Seq2[string, string]
	GetArgsCount() int
	GetArg(k string) (string, bool)
	GetPackage() string
	GetName() string
}

type RefableOut interface {
	Refable
	GetOutput() *string
	GetFilters() []string
}

func FormatFile(pkg string, file string) string {
	return Format(NewRef(JoinPackage("@heph/file", pkg), "content", map[string]string{"f": file}))
}

var formatCache = cache.New[uint64, func() string](cache.AsLFU[uint64, func() string](lfu.WithCapacity(10000)))

var formatHashPool = hsync.Pool[*xxh3.Hasher]{New: xxh3.New}

func sumRef(ref Refable) uint64 {
	h := formatHashPool.Get()
	defer formatHashPool.Put(h)
	h.Reset()

	ref.HashPB(h, nil)

	return h.Sum64()
}

func Format(ref Refable) string {
	sum := sumRef(ref)

	f, _ := formatCache.GetOrSet(sum, sync.OnceValue(func() string { return format(ref) }))

	return f()
}

func format(ref Refable) string {
	var sb strings.Builder
	sb.WriteString("//")
	sb.WriteString(ref.GetPackage())
	sb.WriteString(":")
	sb.WriteString(ref.GetName())

	if ref.GetArgsCount() > 0 {
		sb.WriteString("@")
		first := true
		for k, v := range ref.GetArgs() {
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
		if out != nil {
			sb.WriteString("|")
			sb.WriteString(*out)
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
