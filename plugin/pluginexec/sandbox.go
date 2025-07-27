package pluginexec

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/htypes"
	"iter"
	"os"
	"path/filepath"
	"slices"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/zeebo/xxh3"

	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func SetupSandbox(
	ctx context.Context,
	t *execv1.Target,
	results []*pluginv1.ArtifactWithOrigin,
	workfs,
	binfs,
	cwdfs,
	outfs hfs.OS,
	setupWd bool,
) ([]*pluginv1.ArtifactWithOrigin, error) {
	ctx, span := tracer.Start(ctx, "SetupSandbox")
	defer span.End()

	err := workfs.MkdirAll("", os.ModePerm)
	if err != nil {
		return nil, err
	}

	err = binfs.MkdirAll("", hfs.ModePerm)
	if err != nil {
		return nil, err
	}

	var listArtifacts []*pluginv1.ArtifactWithOrigin
	if setupWd {
		err = cwdfs.MkdirAll("", os.ModePerm)
		if err != nil {
			return nil, err
		}

		for _, dep := range hmaps.Concat(t.GetDeps(), t.GetRuntimeDeps()) {
			for _, target := range dep.GetTargets() {
				for artifact := range ArtifactsForId(results, target.GetId(), 1) {
					listArtifact, err := SetupSandboxArtifact(ctx, artifact.GetArtifact(), workfs, target.GetRef().GetFilters())
					if err != nil {
						return nil, fmt.Errorf("setup artifact: %v: %w", target.GetId(), err)
					}
					listArtifacts = append(listArtifacts, pluginv1.ArtifactWithOrigin_builder{
						Artifact: listArtifact,
						Origin: pluginv1.TargetDef_InputOrigin_builder{
							Id: htypes.Ptr(target.GetId()),
						}.Build(),
					}.Build())
				}
			}

			// TODO: files
		}
	}

	for _, tool := range t.GetTools() {
		for artifact := range ArtifactsForId(results, tool.GetId(), 1) {
			err := SetupSandboxBinArtifact(ctx, artifact.GetArtifact(), binfs)
			if err != nil {
				return nil, fmt.Errorf("%v: %w", tref.Format(tool.GetRef()), err)
			}
		}
	}

	for _, output := range t.GetOutputs() {
		for _, path := range output.GetPaths() {
			err := hfs.CreateParentDir(outfs, path)
			if err != nil {
				return nil, err
			}
		}
	}

	return listArtifacts, nil
}

func ArtifactsForId(inputs []*pluginv1.ArtifactWithOrigin, id string, typ pluginv1.Artifact_Type) iter.Seq[*pluginv1.ArtifactWithOrigin] {
	return func(yield func(origin *pluginv1.ArtifactWithOrigin) bool) {
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

func SetupSandboxArtifact(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS, filters []string) (*pluginv1.Artifact, error) {
	ctx, span := tracer.Start(ctx, "SetupSandboxArtifact")
	defer span.End()

	h := xxh3.New()
	hashpb.Hash(h, artifact, nil)

	listf, err := hfs.Create(fs, hex.EncodeToString(h.Sum(nil))+".list")
	if err != nil {
		return nil, fmt.Errorf("create list file: %w", err)
	}
	defer listf.Close()

	err = hartifact.Unpack(ctx, artifact, fs, hartifact.WithOnFile(func(to string) {
		_, _ = listf.Write([]byte(to))
		_, _ = listf.Write([]byte("\n"))
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

	return pluginv1.Artifact_builder{
		Group: htypes.Ptr(artifact.GetGroup()),
		Name:  htypes.Ptr(artifact.GetName() + ".list"),
		Type:  htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT_LIST_V1),
		File: pluginv1.Artifact_ContentFile_builder{
			SourcePath: htypes.Ptr(listf.Name()),
		}.Build(),
	}.Build(), nil
}

func SetupSandboxBinArtifact(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS) error {
	ctx, span := tracer.Start(ctx, "SetupSandboxBinArtifact")
	defer span.End()

	dir, err := os.MkdirTemp("", "")
	if err != nil {
		return err
	}
	defer func() {
		_ = os.RemoveAll(dir)
	}()

	tmpfs := hfs.NewOS(dir)

	var dest string
	var count int
	err = hartifact.Unpack(ctx, artifact, tmpfs, hartifact.WithOnFile(func(to string) {
		dest = to
		count++
	}))
	if err != nil {
		return err
	}

	if count != 1 {
		return errors.New("must output exactly one file")
	}

	name := filepath.Base(dest)

	err = fs.Move(dest, fs.Path(name))
	if err != nil {
		return err
	}

	return nil
}
