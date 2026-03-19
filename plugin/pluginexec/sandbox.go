package pluginexec

import (
	"context"
	"encoding/hex"
	"fmt"
	"iter"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"sync/atomic"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/zeebo/xxh3"

	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type SetupSandboxResult struct {
	ListArtifacts []*pluginsdk.ArtifactWithOrigin
	Sourcemap     map[string]string
}

func SetupSandbox(
	ctx context.Context,
	t *execv1.Target,
	results []*pluginsdk.ArtifactWithOrigin,
	workfs,
	binfs,
	cwdfs,
	outfs hfs.OS,
	setupWd bool,
) (*SetupSandboxResult, error) {
	ctx, span := tracer.Start(ctx, "SetupSandbox")
	defer span.End()

	err := workfs.MkdirAll(os.ModePerm)
	if err != nil {
		return nil, err
	}

	err = binfs.MkdirAll(hfs.ModePerm)
	if err != nil {
		return nil, err
	}

	sourcemap := map[string]string{}

	var listArtifacts []*pluginsdk.ArtifactWithOrigin
	if setupWd {
		err = cwdfs.MkdirAll(os.ModePerm)
		if err != nil {
			return nil, err
		}

		for _, target := range t.GetDeps() {
			for artifact := range ArtifactsForId(results, target.GetId(), pluginv1.Artifact_TYPE_OUTPUT) {
				listArtifact, err := SetupSandboxArtifact(ctx, artifact, target, workfs, target.GetRef().GetFilters(), sourcemap)
				if err != nil {
					return nil, fmt.Errorf("setup artifact: %v: %w", target.GetId(), err)
				}
				listArtifacts = append(listArtifacts, &pluginsdk.ArtifactWithOrigin{
					Artifact: listArtifact,
					Origin: pluginv1.TargetDef_InputOrigin_builder{
						Id: htypes.Ptr(target.GetId()),
					}.Build(),
				})
			}

			for artifact := range ArtifactsForId(results, target.GetId(), pluginv1.Artifact_TYPE_SUPPORT_FILE) {
				listArtifact, err := SetupSandboxArtifact(ctx, artifact, target, workfs, nil, nil)
				if err != nil {
					return nil, fmt.Errorf("setup support file artifact: %v: %w", target.GetId(), err)
				}
				listArtifacts = append(listArtifacts, &pluginsdk.ArtifactWithOrigin{
					Artifact: listArtifact,
					Origin: pluginv1.TargetDef_InputOrigin_builder{
						Id: htypes.Ptr(target.GetId()),
					}.Build(),
				})
			}
		}
	}

	for _, tool := range t.GetTools() {
		for artifact := range ArtifactsForId(results, tool.GetId(), pluginv1.Artifact_TYPE_OUTPUT) {
			listArtifact, err := SetupSandboxBinArtifact(ctx, artifact, binfs)
			if err != nil {
				return nil, fmt.Errorf("%v: %w", tref.FormatOut(tool.GetRef()), err)
			}
			listArtifacts = append(listArtifacts, &pluginsdk.ArtifactWithOrigin{
				Artifact: listArtifact,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Id: htypes.Ptr(tool.GetId()),
				}.Build(),
			})
		}

		for artifact := range ArtifactsForId(results, tool.GetId(), pluginv1.Artifact_TYPE_SUPPORT_FILE) {
			listArtifact, err := SetupSandboxArtifact(ctx, artifact, nil, workfs, nil, nil)
			if err != nil {
				return nil, fmt.Errorf("setup support file artifact: %v: %w", tref.FormatOut(tool.GetRef()), err)
			}
			listArtifacts = append(listArtifacts, &pluginsdk.ArtifactWithOrigin{
				Artifact: listArtifact,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Id: htypes.Ptr(tool.GetId()),
				}.Build(),
			})
		}
	}

	for _, output := range t.GetOutputs() {
		for _, path := range output.GetPaths() {
			err := hfs.CreateParentDir(outfs.At(path))
			if err != nil {
				return nil, err
			}
		}
	}

	return &SetupSandboxResult{
		ListArtifacts: listArtifacts,
		Sourcemap:     sourcemap,
	}, nil
}

func ArtifactsForId(inputs []*pluginsdk.ArtifactWithOrigin, id string, typ pluginv1.Artifact_Type) iter.Seq[*pluginsdk.ArtifactWithOrigin] {
	return func(yield func(origin *pluginsdk.ArtifactWithOrigin) bool) {
		for _, input := range inputs {
			if input.GetArtifact().GetType() != typ {
				continue
			}

			if input.GetOrigin().GetId() != id {
				continue
			}

			if !yield(input) {
				return
			}
		}
	}
}

var artifactId atomic.Int64

func SetupSandboxArtifact(ctx context.Context, artifact pluginsdk.Artifact, source *execv1.Target_Dep, workfs hfs.Node, filters []string, sourcemap map[string]string) (pluginsdk.Artifact, error) {
	ctx, span := tracer.Start(ctx, "SetupSandboxArtifact")
	defer span.End()

	h := xxh3.New()
	_, _ = h.WriteString(strconv.FormatInt(artifactId.Add(1), 10))

	listf, err := hfs.Create(workfs.At(hex.EncodeToString(h.Sum(nil)) + ".list"))
	if err != nil {
		return nil, fmt.Errorf("create list file: %w", err)
	}
	defer listf.Close()

	writeNl := false

	err = hartifact.Unpack(ctx, artifact, workfs, hartifact.WithOnFile(func(to string) {
		if writeNl {
			_, _ = listf.Write([]byte("\n"))
		}
		_, _ = listf.Write([]byte(to))
		writeNl = true

		if sourcemap != nil {
			rel, err := filepath.Rel(workfs.Path(), to)
			if err != nil {
				panic(err)
			}

			sourcemap[rel] = tref.FormatOut(source.GetRef())
		}
	}), hartifact.WithFilter(func(from string) bool {
		if len(filters) == 0 {
			return true
		}

		return slices.Contains(filters, from)
	}))
	if err != nil {
		return nil, fmt.Errorf("unpack: %w", err)
	}

	err = listf.Close()
	if err != nil {
		return nil, err
	}

	// Determine the list type based on the input artifact type
	listType := pluginv1.Artifact_TYPE_OUTPUT_LIST_V1
	if artifact.GetType() == pluginv1.Artifact_TYPE_SUPPORT_FILE {
		listType = pluginv1.Artifact_TYPE_SUPPORT_FILE_LIST_V1
	}

	return &pluginsdk.ProtoArtifact{
		Artifact: pluginv1.Artifact_builder{
			Group: htypes.Ptr(artifact.GetGroup()),
			Name:  htypes.Ptr(artifact.GetName() + ".list"),
			Type:  htypes.Ptr(listType),
			File: pluginv1.Artifact_ContentFile_builder{
				SourcePath: htypes.Ptr(listf.Name()),
			}.Build(),
		}.Build(),
	}, nil
}

func SetupSandboxBinArtifact(ctx context.Context, artifact pluginsdk.Artifact, node hfs.Node) (pluginsdk.Artifact, error) {
	ctx, span := tracer.Start(ctx, "SetupSandboxBinArtifact")
	defer span.End()

	dir, err := os.MkdirTemp("", "")
	if err != nil {
		return nil, err
	}
	defer func() {
		_ = os.RemoveAll(dir)
	}()

	tmpfs := hfs.NewOS(dir)

	var binPath string
	var count int
	err = hartifact.Unpack(ctx, artifact, tmpfs, hartifact.WithOnFile(func(to string) {
		binPath = to
		count++
	}))
	if err != nil {
		return nil, err
	}

	if count != 1 {
		return nil, fmt.Errorf("must output exactly one file, got %v", count)
	}

	name := filepath.Base(binPath)

	dest := node.At(name)
	err = tmpfs.At(binPath).Move(dest)
	if err != nil {
		return nil, err
	}

	return &pluginsdk.ProtoArtifact{
		Artifact: pluginv1.Artifact_builder{
			Group: htypes.Ptr(artifact.GetGroup()),
			Name:  htypes.Ptr(artifact.GetName() + ".list"),
			Type:  htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT_LIST_V1),
			Raw: pluginv1.Artifact_ContentRaw_builder{
				Data: []byte(dest.Path()),
				Path: htypes.Ptr(artifact.GetName() + ".list"),
			}.Build(),
		}.Build(),
	}, nil
}
