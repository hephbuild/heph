package hbuiltin

import (
	"bufio"
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xstarlark"
	"go.starlark.net/starlark"
	"math"
	"runtime"
	"strconv"
	"strings"
	"time"
	"unicode"
)

func specFromArgs(args TargetArgs, pkg *packages.Package) (specs.Target, error) {
	t := specs.Target{
		Addr:                pkg.TargetAddr(args.Name),
		Name:                args.Name,
		Run:                 args.Run,
		Doc:                 docFromArg(args.Doc),
		FileContent:         []byte(args.FileContent),
		ConcurrentExecution: args.ConcurrentExecution,
		Entrypoint:          args.Entrypoint,
		Package:             pkg,
		PassArgs:            args.PassArgs,
		Cache: specs.Cache{
			Enabled: args.Cache.Enabled,
			Named:   args.Cache.Named.Array,
			History: args.Cache.History,
		},
		RestoreCache: specs.RestoreCache{
			Enabled: args.RestoreCache.Enabled,
			Key:     args.RestoreCache.Key,
			Paths:   args.RestoreCache.Paths,
			Env:     args.RestoreCache.Env,
		},
		Sandbox:        args.SandboxEnabled,
		OutInSandbox:   args.OutInSandbox,
		Codegen:        args.Codegen,
		Labels:         args.Labels,
		Env:            args.Env.ArrMap,
		PassEnv:        args.PassEnv,
		RuntimePassEnv: args.RuntimePassEnv,
		RunInCwd:       args.RunInCwd,
		RuntimeEnv:     args.RuntimeEnv.ArrMap,
		SrcEnv: specs.SrcEnv{
			Default: args.SrcEnv.Default,
			Named:   args.SrcEnv.Named,
		},
		OutEnv:      args.OutEnv,
		HashFile:    args.HashFile,
		GenDepsMeta: args.GenDepsMeta,
	}

	var err error

	t.Gen, err = ads.MapE(sets.NewIdentitySetFrom(args.Gen).Slice(), func(s string) (specs.Matcher, error) {
		return specs.ParseMatcher(s)
	})
	if err != nil {
		return specs.Target{}, err
	}

	for i, m := range t.Gen {
		if !(specs.IsAddrMatcher(m) || specs.IsLabelMatcher(m)) {
			return specs.Target{}, fmt.Errorf("gen[%v]: must be an addr matcher or a label matcher, got %T", i, m)
		}
	}

	t.Tools, err = toolsSpecFromArgs(t, args.Tools)
	if err != nil {
		return specs.Target{}, err
	}

	t.Deps, err = depsSpecFromArgs(t, args.Deps)
	if err != nil {
		return specs.Target{}, err
	}

	t.HashDeps, err = depsSpecFromArgs(t, args.HashDeps)
	if err != nil {
		return specs.Target{}, err
	}

	t.RuntimeDeps, err = depsSpecFromArgs(t, args.RuntimeDeps)
	if err != nil {
		return specs.Target{}, err
	}

	t.Platforms, err = ads.MapE(args.Platforms, platformFromArgs)
	if err != nil {
		return specs.Target{}, err
	}

	t.Transitive.Tools, err = toolsSpecFromArgs(t, args.Transitive.Tools)
	if err != nil {
		return specs.Target{}, err
	}

	t.Transitive.Deps, err = depsSpecFromArgs(t, args.Transitive.Deps)
	if err != nil {
		return specs.Target{}, err
	}

	t.Transitive.HashDeps, err = depsSpecFromArgs(t, args.Transitive.HashDeps)
	if err != nil {
		return specs.Target{}, err
	}

	t.Transitive.RuntimeDeps, err = depsSpecFromArgs(t, args.Transitive.RuntimeDeps)
	if err != nil {
		return specs.Target{}, err
	}

	t.Transitive.Env = args.Transitive.Env.ArrMap
	t.Transitive.RuntimeEnv = args.Transitive.RuntimeEnv.ArrMap
	t.Transitive.PassEnv = args.Transitive.PassEnv
	t.Transitive.RuntimePassEnv = args.Transitive.RuntimePassEnv
	t.Transitive.Platforms, err = ads.MapE(args.Transitive.Platforms, platformFromArgs)
	if err != nil {
		return specs.Target{}, err
	}

	if len(args.Out.ArrMap) > 0 {
		for k, vs := range args.Out.ArrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			for _, v := range vs {
				t.Out = append(t.Out, specs.OutFile{
					Name: k,
					Path: v,
				})
			}
		}
	} else if len(args.Out.ArrMap) > 0 {
		for k, v := range args.Out.ArrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			for _, v := range v {
				t.Out = append(t.Out, specs.OutFile{
					Name: k,
					Path: v,
				})
			}
		}
	} else {
		for _, file := range args.Out.Array {
			t.Out = append(t.Out, specs.OutFile{
				Path: file,
			})
		}
	}

	if len(args.SupportFiles) > 0 {
		t.HasSupportFiles = true

		for _, file := range args.SupportFiles {
			t.Out = append(t.Out, specs.OutFile{
				Name: specs.SupportFilesOutput,
				Path: file,
			})
		}
	}

	ads.SortP(t.Out,
		func(i, j *specs.OutFile) int {
			return strings.Compare(i.Path, j.Path)
		},
		func(i, j *specs.OutFile) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	if t.SrcEnv.Default == "" {
		if t.OutInSandbox {
			t.SrcEnv.Default = specs.FileEnvAbs
		} else {
			t.SrcEnv.Default = specs.FileEnvRelPkg
		}
	}

	t.Annotations = make(map[string]interface{}, len(args.Annotations.Items()))
	for _, item := range args.Annotations.Items() {
		t.Annotations[item.Key] = utils.FromStarlark(item.Value)
	}

	t.Requests = map[string]float64{
		"cpu":    1,
		"memory": 100000000, // 1M
	}
	for _, item := range args.Requests.Items() {
		v, err := requestFromArg(item.Value)
		if err != nil {
			return specs.Target{}, fmt.Errorf("request: %v: %w", item.Key, err)
		}

		t.Requests[item.Key] = v
	}

	if t.OutEnv == "" {
		if t.OutInSandbox {
			t.OutEnv = specs.FileEnvAbs
		} else {
			t.OutEnv = specs.FileEnvRelPkg
		}
	}

	if t.RestoreCache.Env == "" {
		t.RestoreCache.Env = specs.FileEnvRelPkg
	}

	if t.HashFile == "" {
		t.HashFile = specs.HashFileContent
	}

	if t.Entrypoint == "" {
		t.Entrypoint = specs.EntrypointBash
	}

	if len(t.Platforms) == 0 {
		t.Platforms = []specs.Platform{{
			Labels: map[string]string{
				"name": "local",
				"os":   runtime.GOOS,
				"arch": runtime.GOARCH,
			},
			Default: true,
		}}
	}

	if t.Codegen == "" {
		t.Codegen = specs.CodegenNone
	}

	if len(args.Timeout) > 0 {
		t.Timeout, err = time.ParseDuration(args.Timeout)
		if err != nil {
			if strings.Contains(err.Error(), "missing unit in duration") {
				v, err := strconv.ParseInt(args.Timeout, 10, 64)
				if err != nil {
					return specs.Target{}, err
				}

				t.Timeout = time.Duration(v) * time.Second
			} else {
				return specs.Target{}, fmt.Errorf("timeout: %w", err)
			}
		}
	}

	return t, nil
}

// https://kubernetes.io/docs/reference/kubernetes-api/common-definitions/quantity/
var units = map[string]float64{
	"E": math.Pow(10, 18),
	"P": math.Pow(10, 15),
	"T": math.Pow(10, 12),
	"G": math.Pow(10, 9),
	"M": math.Pow(10, 6),
	"k": math.Pow(10, 3),

	"Ei": math.Pow(2, 60),
	"Pi": math.Pow(2, 50),
	"Ti": math.Pow(2, 40),
	"Gi": math.Pow(2, 30),
	"Mi": math.Pow(2, 20),
	"Ki": math.Pow(2, 10),

	"m": math.Pow(10, -3),
}

func requestFromArg(s starlark.Value) (float64, error) {
	switch v := utils.FromStarlark(s).(type) {
	case string:
		mul := float64(1)
		for u, uv := range units {
			rest, ok := strings.CutSuffix(v, u)
			if ok {
				mul = uv
				v = rest
				break
			}
		}

		vi, err := strconv.ParseFloat(v, 10)
		if err != nil {
			return 0, err
		}

		return vi * mul, nil
	case int64:
		return float64(v), nil
	case float64:
		return v, nil
	default:
		return 0, fmt.Errorf("unsupported type %T: %v", v, v)
	}
}

func docFromArg(doc string) string {
	if len(doc) == 0 {
		return ""
	}

	var sb strings.Builder
	var setuped bool
	var prefix string
	scanner := bufio.NewScanner(strings.NewReader(doc))
	for scanner.Scan() {
		line := scanner.Text()

		if !setuped {
			if len(strings.TrimSpace(line)) == 0 {
				continue
			}

			i := strings.IndexFunc(line, func(r rune) bool {
				return !unicode.IsSpace(r)
			})
			if i >= 0 {
				prefix = line[:i]
			}

			setuped = true
		}

		line = strings.TrimPrefix(line, prefix)

		line = strings.TrimRightFunc(line, unicode.IsSpace)

		sb.WriteString(line)
		sb.WriteString("\n")
	}

	if err := scanner.Err(); err != nil {
		panic(err)
	}

	return strings.TrimSpace(sb.String()) + "\n"
}

func platformFromArgs(d xstarlark.Distruct) (specs.Platform, error) {
	labels := map[string]string{}
	options := map[string]interface{}{}
	for _, item := range d.Items() {
		k := item.Key
		v := item.Value

		if k == "options" {
			od, err := xstarlark.UnpackDistruct(v)
			if err != nil {
				return specs.Platform{}, err
			}

			for _, t := range od.Items() {
				options[t.Key] = utils.FromStarlark(t.Value)
			}

			continue
		}
		vs, ok := v.(starlark.String)
		if !ok {
			return specs.Platform{}, fmt.Errorf("%v is %v, expected string", k, v.String())
		}
		labels[k] = vs.GoString()
	}

	return specs.Platform{
		Labels:  labels,
		Options: options,
	}, nil
}

func depsSpecFromArr(t specs.Target, arr []string, name string) (specs.Deps, error) {
	td := specs.Deps{}

	for _, dep := range arr {
		if expr, err := exprs.Parse(dep); err == nil {
			td.Exprs = append(td.Exprs, specs.DepExpr{
				Name: name,
				Expr: expr,
			})
			continue
		}

		if dtp, options, err := specs.TargetOutputOptionsParse(t.Package.Path, dep); err == nil {
			tspec := specs.DepTarget{
				Name:   name,
				Target: dtp.TargetAddr.Full(),
				Output: dtp.Output,
				Mode:   specs.DepModeCopy,
			}

			for k, v := range options {
				switch k {
				case "mode":
					mode := specs.DepMode(v)
					tspec.Mode = mode
				default:
					return specs.Deps{}, fmt.Errorf("invalid option %v=%v", k, v)
				}
			}
			td.Targets = append(td.Targets, tspec)
			continue
		}

		// Is probably file
		td.Files = append(td.Files, specs.DepFile{
			Name: name,
			Path: dep,
		})
	}

	return td, nil
}

func toolsSpecFromString(t specs.Target, ts *specs.Tools, name, tool string) error {
	expr, err := exprs.Parse(tool)
	if err == nil {
		ts.Exprs = append(ts.Exprs, specs.ExprTool{
			Name: name,
			Expr: expr,
		})
		return nil
	}

	tp, err := specs.TargetOutputParse(t.Package.Path, tool)
	if err == nil {
		ts.Targets = append(ts.Targets, specs.TargetTool{
			Name:   name,
			Target: tp.TargetAddr.Full(),
			Output: tp.Output,
		})
		return nil
	}

	if name == "" {
		name = tool
	}

	ts.Hosts = append(ts.Hosts, specs.HostTool{
		Name:    name,
		BinName: tool,
	})

	return nil
}

func toolsSpecFromArgs(t specs.Target, tools ArrayMapStr) (specs.Tools, error) {
	ts := specs.Tools{}

	if len(tools.ArrMap) > 0 {
		for name, s := range tools.ArrMap {
			err := toolsSpecFromString(t, &ts, name, s)
			if err != nil {
				return specs.Tools{}, err
			}
		}
	} else {
		for _, s := range tools.Array {
			err := toolsSpecFromString(t, &ts, "", s)
			if err != nil {
				return specs.Tools{}, err
			}
		}
	}

	return ts, nil
}

func depsSpecFromArgs(t specs.Target, deps ArrayMapStrArray) (specs.Deps, error) {
	var td specs.Deps
	if len(deps.ArrMap) > 0 {
		for name, arr := range deps.ArrMap {
			d, err := depsSpecFromArr(t, arr, name)
			if err != nil {
				return d, err
			}
			td.Targets = append(td.Targets, d.Targets...)
			td.Exprs = append(td.Exprs, d.Exprs...)
			td.Files = append(td.Files, d.Files...)
		}
	} else {
		d, err := depsSpecFromArr(t, deps.Array, "")
		if err != nil {
			return d, err
		}

		td = d
	}

	ads.SortP(td.Exprs,
		func(i, j *specs.DepExpr) int {
			return strings.Compare(i.Expr.String, j.Expr.String)
		},
		func(i, j *specs.DepExpr) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	ads.SortP(td.Targets,
		func(i, j *specs.DepTarget) int {
			return strings.Compare(i.Target, j.Target)
		},
		func(i, j *specs.DepTarget) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	ads.SortP(td.Files,
		func(i, j *specs.DepFile) int {
			return strings.Compare(i.Path, j.Path)
		},
		func(i, j *specs.DepFile) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	return td, nil
}
