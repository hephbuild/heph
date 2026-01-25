package engine

import (
	"context"
	"fmt"
	"path/filepath"
	"slices"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginfs"
)

func (e *Engine) codegenTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	step, ctx := hstep.New(ctx, "Copying to tree...")
	defer step.Done()

	err := e.codegenTreeCopy(ctx, def, outputs, pluginv1.TargetDef_Path_CODEGEN_MODE_COPY)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	err = e.codegenTreeCopy(ctx, def, outputs, pluginv1.TargetDef_Path_CODEGEN_MODE_LINK) // TODO: this will copy too
	if err != nil {
		return fmt.Errorf("link: %w", err)
	}

	return nil
}

func (e *Engine) codegenTreeCopy(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact, mode pluginv1.TargetDef_Path_CodegenMode) error {
	codegenPaths := make([]string, 0)
	for _, output := range def.GetOutputs() {
		for _, path := range output.GetPaths() {
			if path.GetCodegenTree() == mode {
				switch path.WhichContent() {
				case pluginv1.TargetDef_Path_FilePath_case:
					codegenPaths = append(codegenPaths, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), path.GetFilePath()))
				case pluginv1.TargetDef_Path_DirPath_case:
					codegenPaths = append(codegenPaths, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), path.GetDirPath()))
				case pluginv1.TargetDef_Path_Glob_case:
					return fmt.Errorf("codegen tree: %v: cannot be a glob", path.GetGlob())
				default:
					return fmt.Errorf("invalid codegen content: %s", path.WhichContent())
				}
			}
		}
	}

	if len(codegenPaths) == 0 {
		return nil
	}

	isUnderCodegenPath := func(p string) bool {
		if slices.Contains(codegenPaths, p) {
			return true
		}

		for _, codegenPath := range codegenPaths {
			if hfs.HasPathPrefix(p, codegenPath) {
				return true
			}
		}

		return false
	}

	for _, output := range outputs {
		if output.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		err := hartifact.Unpack(ctx, output.Artifact, e.Root, hartifact.WithFilter(isUnderCodegenPath), hartifact.WithOnFile(func(to string) {
			err := pluginfs.MarkCodegen(ctx, def.GetRef(), to)
			if err != nil {
				hlog.From(ctx).Warn("mark codegen", "target", tref.Format(def.GetRef()), "err", err)
			}
		}))
		if err != nil {
			return fmt.Errorf("unpack: %v: %w", output.GetGroup(), err)
		}
	}

	return nil
}
