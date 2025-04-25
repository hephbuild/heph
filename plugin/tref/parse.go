package tref

import (
	"strings"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref/internal"
)

func argsToProto(args []internal.Arg) map[string]string {
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

func Parse(s string) (*pluginv1.TargetRef, error) {
	return parse(s, "", false)
}

func parse(s, pkg string, optPkg bool) (*pluginv1.TargetRef, error) {
	res, err := internal.Parse(s, pkg, optPkg)
	if err != nil {
		return nil, err
	}

	return &pluginv1.TargetRef{
		Package: res.Pkg,
		Name:    res.Name,
		Args:    argsToProto(res.Args),
	}, nil
}

func ParseWithOut(s string) (*pluginv1.TargetRefWithOutput, error) {
	res, err := internal.ParseWithOut(s)
	if err != nil {
		return nil, err
	}

	return &pluginv1.TargetRefWithOutput{
		Package: res.Pkg,
		Name:    res.Name,
		Output:  res.Out,
		Args:    argsToProto(res.Args),
	}, nil
}
