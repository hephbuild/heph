package tref

import (
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"unsafe"

	"github.com/hephbuild/heph/internal/hsync"

	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lfu"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

type Ref = pluginv1.TargetRef
type RefOut = pluginv1.TargetRefWithOutput

type HashStore interface {
	GetHash() uint64
	HasHash() bool
	SetHash(uint64)
}

func FormatFile(pkg string, file string) string {
	return Format(New(JoinPackage(FilePackage, pkg), "content", map[string]string{"f": file}))
}

func ParseFile(ref *pluginv1.TargetRef) (string, bool) {
	rest, ok := CutPackagePrefix(ref.GetPackage(), FilePackage)
	if !ok {
		return "", false
	}

	f := ref.GetArgs()["f"]

	return filepath.Join(ToOSPath(rest), f), true
}

const FilePackage = "@heph/file"
const BinPackage = "@heph/bin"
const QueryPackage = "@heph/query"
const QueryName = "query"

type QueryOptions struct {
	Label        string
	Package      string
	TreeOutputTo string
	SkipProvider string
}

func FormatQuery(o QueryOptions) string {
	m := map[string]string{}
	if o.Label != "" {
		m["label"] = o.Label
	}
	if o.Package != "" {
		m["package"] = o.Package
	}
	if o.TreeOutputTo != "" {
		m["tree_output_to"] = o.TreeOutputTo
	}
	if o.SkipProvider != "" {
		m["skip_provider"] = o.SkipProvider
	}

	return Format(New(QueryPackage, QueryName, m))
}

func ParseQuery(ref *pluginv1.TargetRef) (QueryOptions, error) {
	if ref.GetPackage() != QueryPackage || ref.GetName() != QueryName {
		return QueryOptions{}, errors.New("invalid query ref")
	}

	var qo QueryOptions

	if label, ok := ref.GetArgs()["label"]; ok {
		qo.Label = label
	}

	if pkg, ok := ref.GetArgs()["package"]; ok {
		qo.Package = pkg
	}

	if treeOutputTo, ok := ref.GetArgs()["tree_output_to"]; ok {
		qo.TreeOutputTo = treeOutputTo
	}

	if p, ok := ref.GetArgs()["skip_provider"]; ok {
		qo.SkipProvider = p
	}

	return qo, nil
}

var formatCache = cache.New[uint64, string](cache.AsLFU[uint64, string](lfu.WithCapacity(10000)))
var formatSf = hsingleflight.Group[uint64, string]{}

var formatOutCache = cache.New[uint64, string](cache.AsLFU[uint64, string](lfu.WithCapacity(10000)))
var formatOutSf = hsingleflight.Group[uint64, string]{}

var formatHashPool = hsync.Pool[*xxh3.Hasher]{New: xxh3.New}

func sumRefTargetRef(m *pluginv1.TargetRef) uint64 {
	hasher := formatHashPool.Get()
	defer formatHashPool.Put(hasher)
	hasher.Reset()

	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetPackage()), len(m.GetPackage())))
	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetName()), len(m.GetName())))
	for k, v := range hmaps.Sorted(m.GetArgs()) {
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(k), len(k)))
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
	}

	return hasher.Sum64()
}

func sumRefTargetRefWithOutput(m *pluginv1.TargetRefWithOutput) uint64 {
	hasher := formatHashPool.Get()
	defer formatHashPool.Put(hasher)
	hasher.Reset()

	{
		m := m.GetTarget()

		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetPackage()), len(m.GetPackage())))
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetName()), len(m.GetName())))
		for k, v := range hmaps.Sorted(m.GetArgs()) {
			_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(k), len(k)))
			_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
		}
	}

	_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(m.GetOutput()), len(m.GetOutput())))
	for _, v := range m.GetFilters() {
		_, _ = hasher.Write(unsafe.Slice(unsafe.StringData(v), len(v)))
	}

	return hasher.Sum64()
}

func Format(ref *Ref) string {
	var sum uint64
	if ref.HasHash() {
		sum = ref.GetHash()
	} else {
		sum = sumRefTargetRef(ref)
		ref.SetHash(sum)
	}

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

func format(ref *Ref) string {
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

	return sb.String()
}

func FormatOut(ref *RefOut) string {
	var sum uint64
	if ref.HasHash() {
		sum = ref.GetHash()
	} else {
		sum = sumRefTargetRefWithOutput(ref)
		ref.SetHash(sum)
	}

	f, ok := formatOutCache.Get(sum)
	if ok {
		return f
	}

	f, _, _ = formatOutSf.Do(sum, func() (string, error) {
		f, ok := formatOutCache.Get(sum)
		if ok {
			return f, nil
		}

		f = formatOut(ref)

		formatOutCache.Set(sum, f)

		return f, nil
	})

	return f

}

func formatOut(ref *RefOut) string {
	if ref.GetOutput() == "" && len(ref.GetFilters()) == 0 {
		return Format(ref.GetTarget())
	}

	var sb strings.Builder

	sb.WriteString(Format(ref.GetTarget()))

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

	return sb.String()
}
