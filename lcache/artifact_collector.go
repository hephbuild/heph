package lcache

import (
	"context"
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

	var err error

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

	return nil
}

func (e *LocalCacheState) collectNamedOutFromTar(ctx context.Context, target *graph.Target, outputs []string) (*ActualOutNamedPaths, error) {
	sizeSum := ads.Reduce(outputs, func(s int64, name string) int64 {
		if s < 0 {
			return -1
		}

		artifact := target.Artifacts.OutTar(name)

		stats, _ := e.ArtifactManifest(ctx, target, artifact)
		if stats.Size <= 0 {
			return -1
		}

		return stats.Size
	}, int64(0))

	c := xprogress.NewCounter()

	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	go func() {
		progressCh := xprogress.NewTicker(ctx, c, 10*time.Millisecond)

		for p := range progressCh {
			status.Emit(ctx, tgt.TargetStatus(target, xmath.FormatPercent("Hydrating output [P]...", math.Round(xmath.Percent(p.N(), sizeSum)))))
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
	r, stats, err := e.UncompressedReaderFromArtifact(artifact, target)
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
				status.Emit(ctx, tgt.TargetStatus(target,
					xmath.FormatPercent("Hydrating output [P]...", math.Round(xmath.Percent(p.N(), stats.Size)))),
				)
			}
		}()
	}

	files, err := tar.UntarList(ctx, r, e.tarListPath(artifact, target), func(read int64) {
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
