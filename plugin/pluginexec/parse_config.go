package pluginexec

import (
	"context"
	"fmt"
	"maps"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hmaps"
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

func ParseConfig[T proto.Message](
	ctx context.Context,
	ref *pluginv1.TargetRef,
	config map[string]*structpb.Value,
	protoTarget func(Spec, *execv1.Target) (T, error),
	filterTool func(ref *pluginv1.TargetRefWithOutput) bool,
) (*pluginv1.TargetDef, error) {
	var targetSpec Spec
	targetSpec.Cache.Remote = true
	targetSpec.Cache.Local = true

	err := hstructpb.DecodeTo(config, &targetSpec)
	if err != nil {
		return nil, err
	}

	target := execv1.Target_builder{
		Run:            targetSpec.Run,
		Deps:           map[string]*execv1.Target_Deps{},
		HashDeps:       map[string]*execv1.Target_Deps{},
		RuntimeDeps:    map[string]*execv1.Target_Deps{},
		Env:            targetSpec.Env,
		RuntimeEnv:     targetSpec.RuntimeEnv,
		PassEnv:        targetSpec.PassEnv,
		RuntimePassEnv: targetSpec.RuntimePassEnv,
		Context:        htypes.Ptr(execv1.Target_SoftSandbox),
	}.Build()

	if targetSpec.InTree {
		target.SetContext(execv1.Target_Tree)
	}

	var codegenPaths []string
	for k, out := range targetSpec.Out {
		target.SetOutputs(append(target.GetOutputs(), execv1.Target_Output_builder{
			Group: htypes.Ptr(k),
			Paths: out,
		}.Build()))

		for _, s := range out {
			if hfs.IsGlob(s) {
				base, _ := hfs.GlobSplit(s)
				if !strings.HasSuffix(base, "/") {
					base += "/"
				}

				codegenPaths = append(codegenPaths, base)
			} else {
				codegenPaths = append(codegenPaths, s)
			}
		}
	}

	for _, output := range target.GetOutputs() {
		slices.SortFunc(output.GetPaths(), strings.Compare)
	}

	slices.SortFunc(target.GetOutputs(), func(a, b *execv1.Target_Output) int {
		return strings.Compare(a.GetGroup(), b.GetGroup())
	})

	collectOutputs := make([]*pluginv1.TargetDef_CollectOutput, 0, len(target.GetOutputs()))
	for _, output := range target.GetOutputs() {
		collectOutputs = append(collectOutputs, pluginv1.TargetDef_CollectOutput_builder{
			Group: htypes.Ptr(output.GetGroup()),
			Paths: output.GetPaths(),
		}.Build())
	}

	var inputs []*pluginv1.TargetDef_Input
	for name, deps := range hmaps.Sorted(targetSpec.Deps.Merge(targetSpec.HashDeps, targetSpec.RuntimeDeps)) {
		var execDeps execv1.Target_Deps
		for i, dep := range deps {
			ref, err := tref.ParseWithOut(dep)
			if err != nil {
				return nil, fmt.Errorf("dep[%v][%d]: %q: %w", name, i, dep, err)
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			id := depId("deps", name, i)
			inputs = append(inputs, pluginv1.TargetDef_Input_builder{
				Ref: ref,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Meta: meta,
					Id:   htypes.Ptr(id),
				}.Build(),
			}.Build())

			execDeps.SetTargets(append(execDeps.GetTargets(), execv1.Target_InputRef_builder{
				Ref: ref,
				Id:  htypes.Ptr(id),
			}.Build()))
		}
		target.GetDeps()[name] = &execDeps
	}

	for _, tools := range targetSpec.Tools {
		for i, tool := range tools {
			ref, err := tref.ParseWithOut(tool)
			if err != nil {
				return nil, fmt.Errorf("tool[%d]: %q: %w", i, tool, err)
			}

			if !filterTool(ref) {
				continue
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			id := depId("tools", "", i)
			inputs = append(inputs, pluginv1.TargetDef_Input_builder{
				Ref: ref,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Meta: meta,
					Id:   htypes.Ptr(id),
				}.Build(),
			}.Build())
			target.SetTools(append(target.GetTools(), execv1.Target_InputRef_builder{
				Ref: ref,
				Id:  htypes.Ptr(id),
			}.Build()))
		}
	}

	hash := xxh3.New()
	desc := target.ProtoReflect().Descriptor()
	hashpb.Hash(hash, target, map[string]struct{}{
		string(desc.FullName()) + ".runtime_deps":     {},
		string(desc.FullName()) + ".runtime_env":      {},
		string(desc.FullName()) + ".runtime_pass_env": {},
	})

	ptarget, err := protoTarget(targetSpec, target)
	if err != nil {
		return nil, err
	}

	targetAny, err := anypb.New(ptarget)
	if err != nil {
		return nil, err
	}

	var codegenTree []*pluginv1.TargetDef_CodegenTree
	switch targetSpec.Codegen {
	case "":
		// no codegen
	case "copy":
		for _, p := range codegenPaths {
			codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
				Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY),
				Path:  htypes.Ptr(p),
				IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
			}.Build())
		}
	case "link":
		for _, p := range codegenPaths {
			codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
				Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_LINK),
				Path:  htypes.Ptr(p),
				IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
			}.Build())
		}
	default:
		return nil, fmt.Errorf("invalid codegen mode: %s", targetSpec.Codegen)
	}

	return pluginv1.TargetDef_builder{
		Ref:                ref,
		Def:                targetAny,
		Inputs:             inputs,
		Outputs:            slices.Collect(maps.Keys(targetSpec.Out)),
		Cache:              htypes.Ptr(targetSpec.Cache.Local),
		DisableRemoteCache: htypes.Ptr(!targetSpec.Cache.Remote),
		CollectOutputs:     collectOutputs,
		CodegenTree:        codegenTree,
		Pty:                htypes.Ptr(targetSpec.Pty),
		Hash:               hash.Sum(nil),
	}.Build(), nil
}
