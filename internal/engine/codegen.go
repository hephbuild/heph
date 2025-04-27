package engine

import (
	"context"
	"fmt"
	"path/filepath"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hartifact"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func (e *Engine) codegenTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	if def.CodegenTree == nil {
		return nil
	}

	switch def.CodegenTree.GetMode() {
	case pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY:
		err := e.codegenCopyTree(ctx, def, outputs)
		if err != nil {
			return fmt.Errorf("copy: %w", err)
		}
	case pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_LINK:
		err := e.codegenLinkTree(ctx, def, outputs)
		if err != nil {
			return fmt.Errorf("link: %w", err)
		}
	case pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_UNSPECIFIED:
	default:
		return fmt.Errorf("unknown codegen mode %v", def.CodegenTree.GetMode())
	}

	return nil
}

func (e *Engine) codegenCopyTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	codegenPaths := make([]string, 0, len(outputs))
	for _, path := range def.CodegenTree.GetPaths() {
		codegenPaths = append(codegenPaths, filepath.Join(def.GetRef().GetPackage(), path))
	}

	isUnderCodegenPath := func(p string) bool {
		if slices.Contains(codegenPaths, p) {
			return true
		}

		for _, codegenPath := range codegenPaths {
			if strings.HasPrefix(p, codegenPath) {
				return true
			}
		}

		return false
	}

	for _, output := range outputs {
		if output.Type != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		err := hartifact.Unpack(ctx, output.Artifact, e.Root, hartifact.WithFilter(isUnderCodegenPath))
		if err != nil {
			return fmt.Errorf("unpack: %v: %w", output.Group, err)
		}
	}

	return nil
}

func (e *Engine) codegenLinkTree(ctx context.Context, def *LightLinkedTarget, outputs []ExecuteResultArtifact) error {
	// TODO

	return e.codegenCopyTree(ctx, def, outputs)
}
