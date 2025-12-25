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

	err := e.codegenCopyTree(ctx, def, outputs)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	err = e.codegenLinkTree(ctx, def, outputs)
	if err != nil {
		return fmt.Errorf("link: %w", err)
	}

	return nil
}

func (e *Engine) codegenCopyTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	if len(def.GetCodegenTree()) == 0 {
		return nil
	}

	codegenPaths := make([]string, 0, len(outputs))
	for _, gen := range def.GetCodegenTree() {
		if gen.GetMode() == pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY {
			codegenPaths = append(codegenPaths, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), gen.GetPath()))
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

func (e *Engine) codegenLinkTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	// TODO

	return e.codegenCopyTree(ctx, def, outputs)
}
