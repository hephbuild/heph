package lcache

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
)

func (e *LocalCacheState) PopulateActualFiles(ctx context.Context, target *Target, outputs []string) error {
	log.Tracef("populateActualFilesFromTar %v", target.Addr)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(xfs.RelPaths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOutFromTar(ctx, target.Target, outputs)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		art := target.Artifacts.OutTar(specs.SupportFilesOutput)

		target.actualSupportFiles, err = e.outputFileListFromArtifact(ctx, target, art)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *LocalCacheState) collectNamedOutFromTar(ctx context.Context, target *graph.Target, outputs []string) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name := range target.Out.Named() {
		if !ads.Contains(outputs, name) {
			continue
		}

		tp.ProvisionName(name)

		artifact := target.Artifacts.OutTar(name)

		files, err := e.outputFileListFromArtifact(ctx, target, artifact)
		if err != nil {
			return nil, err
		}

		tp.AddAll(name, files)
	}

	tp.Sort()

	return tp, nil
}

func (e *LocalCacheState) outputFileListFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (xfs.RelPaths, error) {
	r, err := e.UncompressedReaderFromArtifact(artifact, target)
	if err != nil {
		return nil, err
	}
	defer r.Close()

	files, err := tar.UntarList(ctx, r, e.tarListPath(artifact, target))
	if err != nil {
		return nil, err
	}

	ps := xfs.RelPaths(ads.Map(files, func(path string) xfs.RelPath {
		return xfs.NewRelPath(path)
	}))

	ps.Sort()

	return ps, nil
}
