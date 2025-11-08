package pluginexec

import (
	"context"
	"fmt"
	"maps"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/anypb"
	"google.golang.org/protobuf/types/known/structpb"
)

func ConfigToExecv1(
	ctx context.Context,
	ref *pluginv1.TargetRef,
	config map[string]*structpb.Value,
	filterTool func(ref *pluginv1.TargetRefWithOutput) bool,
) (*execv1.Target, []byte, error) {
	var targetSpec Spec
	targetSpec.Cache.Remote = true
	targetSpec.Cache.Local = true

	err := hstructpb.DecodeTo(config, &targetSpec)
	if err != nil {
		return nil, nil, err
	}

	tbuild := execv1.Target_builder{
		Run:         targetSpec.Run,
		LocalCache:  htypes.Ptr(targetSpec.Cache.Local),
		RemoteCache: htypes.Ptr(targetSpec.Cache.Remote),
		Pty:         htypes.Ptr(targetSpec.Pty),
	}

	if targetSpec.InTree {
		tbuild.Context = htypes.Ptr(execv1.Target_CONTEXT_TREE)
	} else {
		tbuild.Context = htypes.Ptr(execv1.Target_CONTEXT_SOFT_SANDBOX)
	}

	for k, out := range targetSpec.Out {
		codegen := execv1.Target_Output_CODEGEN_MODE_UNSPECIFIED
		switch targetSpec.Codegen {
		case "":
			// no codegen
		case "copy":
			codegen = execv1.Target_Output_CODEGEN_MODE_COPY
		case "link":
			codegen = execv1.Target_Output_CODEGEN_MODE_LINK
		default:
			return nil, nil, fmt.Errorf("invalid codegen mode: %s", targetSpec.Codegen)
		}

		tbuild.Outputs = append(tbuild.Outputs, execv1.Target_Output_builder{
			Group:   htypes.Ptr(k),
			Paths:   out,
			Codegen: htypes.Ptr(codegen),
		}.Build())
	}
	slices.SortFunc(tbuild.Outputs, func(a, b *execv1.Target_Output) int {
		return strings.Compare(a.GetGroup(), b.GetGroup())
	})

	processDep := func(id, name, dep string, runtime, hash bool) error {
		ref, err := tref.ParseWithOut(dep)
		if err != nil {
			return err
		}

		tbuild.Deps = append(tbuild.Deps, execv1.Target_Dep_builder{
			Ref:     ref,
			Link:    nil,
			Hash:    htypes.Ptr(hash),
			Runtime: htypes.Ptr(runtime),
			Group:   htypes.Ptr(name),
			Id:      htypes.Ptr(id),
		}.Build())

		return nil
	}

	for name, deps := range targetSpec.Deps {
		for i, dep := range deps {
			err := processDep(depId("deps", name, i), name, dep, true, true)
			if err != nil {
				return nil, nil, fmt.Errorf("dep[%v][%d]: %q: %w", name, i, dep, err)
			}
		}
	}
	for name, deps := range targetSpec.RuntimeDeps {
		for i, dep := range deps {
			err := processDep(depId("runtime_dep", name, i), name, dep, true, false)
			if err != nil {
				return nil, nil, fmt.Errorf("runtime_dep[%v][%d]: %q: %w", name, i, dep, err)
			}
		}
	}
	for name, deps := range targetSpec.HashDeps {
		for i, dep := range deps {
			err := processDep(depId("hash_dep", name, i), name, dep, false, true)
			if err != nil {
				return nil, nil, fmt.Errorf("hash_dep[%v][%d]: %q: %w", name, i, dep, err)
			}
		}
	}
	slices.SortFunc(tbuild.Deps, func(a, b *execv1.Target_Dep) int {
		return strings.Compare(a.GetId(), b.GetId())
	})

	for name, tools := range targetSpec.Tools {
		for i, tool := range tools {
			ref, err := tref.ParseWithOut(tool)
			if err != nil {
				return nil, nil, fmt.Errorf("tool[%d]: %q: %w", i, tool, err)
			}

			if filterTool != nil && !filterTool(ref) {
				continue
			}

			id := depId("tools", name, i)

			tbuild.Tools = append(tbuild.Tools, execv1.Target_Tool_builder{
				Ref:   ref,
				Hash:  htypes.Ptr(true),
				Group: htypes.Ptr(""),
				Id:    htypes.Ptr(id),
			}.Build())
		}
	}
	slices.SortFunc(tbuild.Tools, func(a, b *execv1.Target_Tool) int {
		return strings.Compare(a.GetId(), b.GetId())
	})

	tbuild.Env = map[string]*execv1.Target_Env{}
	for k, v := range targetSpec.Env {
		tbuild.Env[k] = execv1.Target_Env_builder{
			Literal: htypes.Ptr(v),
			Hash:    htypes.Ptr(true),
		}.Build()
	}
	for k, v := range targetSpec.RuntimeEnv {
		tbuild.Env[k] = execv1.Target_Env_builder{
			Literal: htypes.Ptr(v),
			Hash:    htypes.Ptr(false),
		}.Build()
	}
	for _, k := range targetSpec.PassEnv {
		tbuild.Env[k] = execv1.Target_Env_builder{
			Pass: htypes.Ptr(true),
			Hash: htypes.Ptr(true),
		}.Build()
	}
	for _, k := range targetSpec.RuntimePassEnv {
		tbuild.Env[k] = execv1.Target_Env_builder{
			Pass: htypes.Ptr(true),
			Hash: htypes.Ptr(false),
		}.Build()
	}

	target := tbuild.Build()

	return target, hashTarget(target), nil
}

func depId(prop string, group string, i int) string {
	return fmt.Sprintf("%q %q %v", prop, group, i)
}

func ToDef[S proto.Message](ref *pluginv1.TargetRef, target S, getTarget func(S) *execv1.Target, hash []byte) (*pluginv1.TargetDef, error) {
	def, err := execv1ToDef(ref, getTarget(target), target, hash)
	if err != nil {
		return nil, err
	}

	return def, nil
}

func hashTarget(target *execv1.Target) []byte {
	target = hproto.Clone(target)

	env := maps.Clone(target.GetEnv())
	maps.DeleteFunc(env, func(s string, env *execv1.Target_Env) bool {
		return !env.GetHash()
	})
	target.SetEnv(env)

	deps := slices.Clone(target.GetDeps())
	slices.DeleteFunc(deps, func(dep *execv1.Target_Dep) bool {
		return !dep.GetHash()
	})
	target.SetDeps(deps)

	h := xxh3.New()
	hashpb.Hash(h, target, nil)

	return h.Sum(nil)
}

func execv1ToDef(ref *pluginv1.TargetRef, target *execv1.Target, targetDef proto.Message, hash []byte) (*pluginv1.TargetDef, error) {
	inputs := make([]*pluginv1.TargetDef_Input, 0, len(target.GetDeps())+len(target.GetTools()))
	for _, dep := range target.GetDeps() {
		inputs = append(inputs, pluginv1.TargetDef_Input_builder{
			Ref: dep.GetRef(),
			Origin: pluginv1.TargetDef_InputOrigin_builder{
				Id: htypes.Ptr(dep.GetId()),
			}.Build(),
		}.Build())
	}

	for _, tool := range target.GetTools() {
		inputs = append(inputs, pluginv1.TargetDef_Input_builder{
			Ref: tool.GetRef(),
			Origin: pluginv1.TargetDef_InputOrigin_builder{
				Id: htypes.Ptr(tool.GetId()),
			}.Build(),
		}.Build())
	}

	collectOutputs := make([]*pluginv1.TargetDef_CollectOutput, 0, len(target.GetOutputs()))
	var codegenTree []*pluginv1.TargetDef_CodegenTree
	outputNames := make([]string, 0, len(target.GetOutputs()))
	for _, output := range target.GetOutputs() {
		collectOutputs = append(collectOutputs, pluginv1.TargetDef_CollectOutput_builder{
			Group: htypes.Ptr(output.GetGroup()),
			Paths: output.GetPaths(),
		}.Build())

		outputNames = append(outputNames, output.GetGroup())

		switch output.GetCodegen() {
		case execv1.Target_Output_CODEGEN_MODE_UNSPECIFIED:
			// no codegen
		case execv1.Target_Output_CODEGEN_MODE_COPY:
			for _, p := range output.GetPaths() {
				codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
					Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY),
					Path:  htypes.Ptr(p),
					IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
				}.Build())
			}
		case execv1.Target_Output_CODEGEN_MODE_LINK:
			for _, p := range output.GetPaths() {
				codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
					Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_LINK),
					Path:  htypes.Ptr(p),
					IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
				}.Build())
			}
		default:
			return nil, fmt.Errorf("invalid codegen mode: %s", output.GetCodegen())
		}
	}

	anyTarget, err := anypb.New(targetDef)
	if err != nil {
		return nil, err
	}

	return pluginv1.TargetDef_builder{
		Ref:                ref,
		Inputs:             inputs,
		Outputs:            outputNames,
		Cache:              htypes.Ptr(target.GetLocalCache()),
		DisableRemoteCache: htypes.Ptr(!target.GetRemoteCache()),
		CollectOutputs:     collectOutputs,
		CodegenTree:        codegenTree,
		Pty:                htypes.Ptr(target.GetPty()),
		Def:                anyTarget,
		Hash:               hash,
	}.Build(), nil
}

func ApplyTransitiveExecv1(ref *pluginv1.TargetRef, sandbox *pluginv1.Sandbox, execTarget *execv1.Target) (*execv1.Target, []byte, error) {
	tools := execTarget.GetTools()
	for _, tool := range sandbox.GetTools() {
		tools = append(tools, execv1.Target_Tool_builder{
			Ref:   tool.GetRef(),
			Group: htypes.Ptr(tool.GetGroup()),
			Hash:  htypes.Ptr(tool.GetHash()),
			Id:    htypes.Ptr(tool.GetId()),
		}.Build())
	}
	execTarget.SetTools(tools)

	deps := execTarget.GetDeps()
	for _, dep := range sandbox.GetDeps() {
		deps = append(deps, execv1.Target_Dep_builder{
			Ref:     dep.GetRef(),
			Link:    nil,
			Group:   htypes.Ptr(dep.GetGroup()),
			Runtime: htypes.Ptr(dep.GetRuntime()),
			Hash:    htypes.Ptr(dep.GetHash()),
			Id:      htypes.Ptr(dep.GetId()),
		}.Build())
	}
	execTarget.SetDeps(deps)

	env := execTarget.GetEnv()
	if env == nil {
		env = map[string]*execv1.Target_Env{}
	}
	for key, envSpec := range sandbox.GetEnv() {
		b := execv1.Target_Env_builder{
			Hash: htypes.Ptr(envSpec.GetHash()),
		}
		switch {
		case envSpec.HasLiteral():
			b.Literal = htypes.Ptr(envSpec.GetLiteral())
		case envSpec.HasPass():
			b.Pass = htypes.Ptr(envSpec.GetPass())
		}

		env[key] = b.Build()
	}
	execTarget.SetEnv(env)

	return execTarget, hashTarget(execTarget), nil
}
