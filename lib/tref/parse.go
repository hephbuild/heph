package tref

import (
	"strings"

	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lru"
	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/lib/tref/internal"
	"github.com/hephbuild/heph/lib/tref/internal/lexer"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func argsToProto(args []lexer.Arg) map[string]string {
	margs := map[string]string{}
	for _, arg := range args {
		v := arg.Value.Ident
		if arg.Value.Str != "" {
			v = strings.ReplaceAll(arg.Value.Str, `\"`, `"`)
		}
		margs[arg.Key] = v
	}

	return margs
}

func ParseInPackage(s, pkg string) (*pluginv1.TargetRef, error) {
	return parse(s, pkg, true)
}

func IsRelative(s string) bool {
	return internal.IsRelative(s)
}

func Parse(s string) (*pluginv1.TargetRef, error) {
	return parse(s, "", false)
}

func parse(s, pkg string, optPkg bool) (*pluginv1.TargetRef, error) {
	res, err := internal.Parse(s, pkg, optPkg)
	if err != nil {
		return nil, err
	}

	return New(res.Pkg, res.Name, argsToProto(res.Args)), nil
}

var parseWithOutCache = cache.New(cache.AsLRU[string, *pluginv1.TargetRefWithOutput](lru.WithCapacity(10000)))

func ParseWithOut(s string) (*pluginv1.TargetRefWithOutput, error) {
	v, ok := parseWithOutCache.Get(s)
	if ok {
		return hproto.Clone(v), nil
	}

	v, err := parseWithOutInner(s)
	if err != nil {
		return nil, err
	}

	parseWithOutCache.Set(s, v)

	return hproto.Clone(v), nil
}

func parseWithOutInner(s string) (*pluginv1.TargetRefWithOutput, error) {
	res, err := internal.ParseWithOut(s) // this one gives better err messages
	if err != nil {
		return nil, err
	}

	var filters []string
	if res.Filters != "" {
		filters = strings.Split(res.Filters, ",")
	}

	rref := New(res.Pkg, res.Name, argsToProto(res.Args))
	if res.Out == nil {
		return WithFilters(WithOut(rref, ""), filters), nil
	} else {
		return WithFilters(WithOut(rref, *res.Out), filters), nil
	}
}
