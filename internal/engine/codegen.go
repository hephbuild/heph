package engine

import (
	"context"
	"fmt"
	"path/filepath"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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
			codegenPaths = append(codegenPaths, filepath.Join(def.GetRef().GetPackage(), gen.GetPath()))
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
			if strings.HasPrefix(p, codegenPath+"/") {
				return true
			}
		}

		return false
	}

	for _, output := range outputs {
		if output.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		err := hartifact.Unpack(ctx, output.Artifact, e.Root, hartifact.WithFilter(isUnderCodegenPath))
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
