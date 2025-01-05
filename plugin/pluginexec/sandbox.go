package pluginexec

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"maps"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hproto"

	"github.com/hephbuild/heph/internal/hiter"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func SetupSandbox(ctx context.Context, t *execv1.Target, results []*pluginv1.ArtifactWithOrigin, workfs, binfs, cwdfs hfs.OS) ([]*pluginv1.ArtifactWithOrigin, error) {
	err := workfs.MkdirAll("", os.ModePerm)
	if err != nil {
		return nil, err
	}

	err = binfs.MkdirAll("", hfs.ModePerm)
	if err != nil {
		return nil, err
	}

	err = cwdfs.MkdirAll("", os.ModePerm)
	if err != nil {
		return nil, err
	}

	var listArtifacts []*pluginv1.ArtifactWithOrigin
	for _, dep := range hiter.Concat2(maps.All(t.GetDeps()), maps.All(t.GetRuntimeDeps())) {
		for _, target := range dep.GetTargets() {
			for artifact := range ArtifactsForDep(results, target) {
				listArtifact, err := SetupSandboxArtifact(ctx, artifact.GetArtifact(), workfs)
				if err != nil {
					return nil, err
				}
				listArtifacts = append(listArtifacts, &pluginv1.ArtifactWithOrigin{
					Artifact: listArtifact,
					Meta:     artifact.GetMeta(),
				})
			}
		}

		// TODO: files
	}

	for _, tool := range t.GetTools() {
		for artifact := range ArtifactsForDep(results, tool) {
			err := SetupSandboxBinArtifact(ctx, artifact.GetArtifact(), binfs)
			if err != nil {
				return nil, fmt.Errorf("%v: %w", artifact.GetMeta(), err)
			}
		}
	}

	for _, output := range t.GetOutputs() {
		for _, path := range output.GetPaths() {
			err := hfs.CreateParentDir(cwdfs, path)
			if err != nil {
				return nil, err
			}
		}
	}

	return listArtifacts, nil
}

func ArtifactsForDep(inputs []*pluginv1.ArtifactWithOrigin, ref *execv1.Target_Deps_TargetRef) iter.Seq[*pluginv1.ArtifactWithOrigin] {
	return func(yield func(origin *pluginv1.ArtifactWithOrigin) bool) {
		for _, input := range inputs {
			if input.GetArtifact().GetType() != pluginv1.Artifact_TYPE_OUTPUT {
				continue
			}

			aref := &execv1.Target_Deps_TargetRef{}
			err := input.GetMeta().UnmarshalTo(aref)
			if err != nil {
				continue
			}

			if !hproto.Equal(aref, ref) {
				continue
			}

			if !yield(input) {
				return
			}
		}
	}
}

func SetupSandboxArtifact(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS) (*pluginv1.Artifact, error) {
	listf, err := hfs.Create(fs, artifact.GetName()+".list")
	if err != nil {
		return nil, err
	}
	defer listf.Close()

	err = hartifact.Unpack(ctx, artifact, fs, hartifact.WithOnFile(func(to string) {
		_, _ = listf.Write([]byte(to))
		_, _ = listf.Write([]byte("\n"))
	}))
	if err != nil {
		return nil, err
	}

	err = listf.Close()
	if err != nil {
		return nil, err
	}

	return &pluginv1.Artifact{
		Group:    artifact.GetGroup(),
		Name:     artifact.GetName() + ".list",
		Type:     pluginv1.Artifact_TYPE_OUTPUT_LIST_V1,
		Encoding: pluginv1.Artifact_ENCODING_NONE,
		Uri:      "file://" + listf.Name(),
	}, nil
}

func SetupSandboxBinArtifact(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS) error {
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
