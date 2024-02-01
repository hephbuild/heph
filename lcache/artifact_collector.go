package lcache

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/utils/xprogress"
	"math"
	"time"
)

func (e *LocalCacheState) PopulateActualFiles(ctx context.Context, target *Target, outputs []string) error {
	log.Tracef("populateActualFilesFromTar %v", target.Addr)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(xfs.RelPaths, 0)
	target.actualRestoreCacheFiles = make(xfs.RelPaths, 0)

	var err error

	outputs = ads.Filter(outputs, func(s string) bool {
		return s != specs.SupportFilesOutput
	})

	target.actualOutFiles, err = e.collectNamedOutFromTar(ctx, target.Target, outputs)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		art := target.Artifacts.OutTar(specs.SupportFilesOutput)

		target.actualSupportFiles, err = e.outputFileListFromArtifact(ctx, target, art, nil)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	if art, ok := target.Artifacts.GetRestoreCache(); ok {
		target.actualRestoreCacheFiles, err = e.outputFileListFromArtifact(ctx, target, art, nil)
		if err != nil && !errors.Is(err, artifacts.ErrNotFound) {
			return fmt.Errorf("cache: %w", err)
		}
	}

	return nil
}

func (e *LocalCacheState) collectNamedOutFromTar(ctx context.Context, target *graph.Target, outputs []string) (*ActualOutNamedPaths, error) {
	sizeSum, err := ads.ReduceE(outputs, func(s int64, name string) (int64, error) {
		if s < 0 {
			return -1, nil
		}

		artifact := target.Artifacts.OutTar(name)

		stats, _, err := e.ArtifactManifest(ctx, target, artifact)
		if err != nil {
			return -1, err
		}
		if stats.Size <= 0 {
			return -1, nil
		}

		return stats.Size, nil
	}, int64(0))
	if err != nil {
		return nil, err
	}

	c := xprogress.NewCounter()

	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	go func() {
		progressCh := xprogress.NewTicker(ctx, c, 10*time.Millisecond)

		for p := range progressCh {
			status.EmitInteractive(ctx, tgt.TargetStatus(target, xmath.FormatPercent("Hydrating output [P]...", math.Round(xmath.Percent(p.N(), sizeSum)))))
		}
	}()

	tp := &ActualOutNamedPaths{}

	for _, name := range outputs {
		tp.ProvisionName(name)

		artifact := target.Artifacts.OutTar(name)

		files, err := e.outputFileListFromArtifact(ctx, target, artifact, c)
		if err != nil {
			return nil, err
		}

		tp.AddAll(name, files)
	}

	tp.Sort()

	return tp, nil
}

func (e *LocalCacheState) outputFileListFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact, u *xprogress.Counter) (xfs.RelPaths, error) {
	r, stats, err := e.UncompressedReaderFromArtifact(ctx, artifact, target)
	if err != nil {
		return nil, err
	}
	defer r.Close()

	if u == nil {
		u = xprogress.NewCounter()

		ctx, cancel := context.WithCancel(ctx)
		defer cancel()
		go func() {
			progressCh := xprogress.NewTicker(ctx, u, 10*time.Millisecond)

			for p := range progressCh {
				status.EmitInteractive(ctx, tgt.TargetStatus(target,
					xmath.FormatPercent("Hydrating output [P]...", math.Round(xmath.Percent(p.N(), stats.Size)))),
				)
			}
		}()
	}

	tarPath, err := e.tarListPath(artifact, target)
	if err != nil {
		return nil, err
	}

	files, err := tar.UntarList(ctx, r, tarPath, func(read int64) {
		u.AddN(read)
	})
	if err != nil {
		return nil, err
	}

	ps := xfs.RelPaths(ads.Map(files, func(path string) xfs.RelPath {
		return xfs.NewRelPath(path)
	}))

	ps.Sort()

	return ps, nil
}
