package lcache

import (
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xmath"
	"io"
	"math"
	"os"
	"path/filepath"
)

func (e *LocalCacheState) LockArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (func(), error) {
	starget := target.Spec()
	l := e.Metas.Find(target).cacheLocks[artifact.Name()]
	err := l.Lock(ctx)
	if err != nil {
		return nil, fmt.Errorf("lock %v %v: %w", starget.Addr, artifact.Name(), err)
	}

	return func() {
		err := l.Unlock()
		if err != nil {
			log.Errorf("unlock %v %v: %v", starget.Addr, artifact.Name(), err)
		}
	}, nil
}

func (e *LocalCacheState) LockArtifacts(ctx context.Context, target graph.Targeter, artifacts []artifacts.Artifact) (func(), error) {
	unlockers := make([]func(), 0, len(artifacts))

	unlocker := func() {
		for _, unlock := range unlockers {
			unlock()
		}
	}

	for _, artifact := range artifacts {
		unlock, err := e.LockArtifact(ctx, target, artifact)
		if err != nil {
			unlocker()
			return nil, err
		}
		unlockers = append(unlockers, unlock)
	}

	return unlocker, nil
}

func (e *LocalCacheState) GenArtifacts(ctx context.Context, gtarget graph.Targeter, arts []ArtifactWithProducer, compress bool) error {
	target := gtarget.GraphTarget()

	unlock, err := e.LockArtifacts(ctx, target, artifacts.ToSlice(arts))
	if err != nil {
		return err
	}
	defer unlock()

	dir := e.cacheDir(target).Abs()

	err = os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	for _, artifact := range arts {
		shouldCompress := artifact.Compressible() && compress

		var progress func(size, written int64)
		if status.IsInteractive(ctx) {
			progress = func(size, written int64) {
				percent := math.Round(xmath.Percent(written, size))

				var s string
				if target.Cache.Enabled {
					s = xmath.FormatPercent("Caching %P...", percent)
				} else if len(target.Artifacts.Out) > 0 {
					s = xmath.FormatPercent("Storing %P...", percent)
				}

				if s != "" {
					status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.Name(), s))
				}
			}
		}

		err := GenArtifact(ctx, dir, artifact, shouldCompress, progress)
		if err != nil {
			return fmt.Errorf("genartifact %v: %w", artifact.Name(), err)
		}
	}

	return nil
}

type ArtifactGenContext struct {
	w              io.WriteCloser
	tracker        xio.Tracker
	accessedWriter bool
	progress       func(size int64, written int64)
}

func (g *ArtifactGenContext) EstimatedWriteSize(size int64) {
	progress := g.progress
	if progress == nil {
		return
	}

	g.tracker.OnWrite = func(written int64) {
		progress(size, written)
	}
}

func (g *ArtifactGenContext) Writer() io.Writer {
	g.accessedWriter = true
	return io.MultiWriter(g.w, &g.tracker)
}

var ArtifactSkip = errors.New("skip artifact")

type ArtifactManifest struct {
	Size int64
}

func GenArtifact(ctx context.Context, dir string, a ArtifactWithProducer, compress bool, progress func(size, written int64)) error {
	compress = a.Compressible() && compress

	p := filepath.Join(dir, a.FileName())
	if compress {
		p = filepath.Join(dir, a.GzFileName())
	}

	tmpp := xfs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	f, err := os.Create(tmpp)
	if err != nil {
		return err
	}
	defer f.Close()

	done := xio.ContextCloser(ctx, f)
	defer done()

	gctx := ArtifactGenContext{
		progress: progress,
	}
	gctx.w = f

	if compress {
		gw := gzip.NewWriter(f)
		defer gw.Close()

		gctx.w = gw
	}

	err = a.Gen(ctx, &gctx)
	if err != nil {
		if errors.Is(err, ArtifactSkip) {
			if a.GenRequired() {
				return fmt.Errorf("is required, but returned: %w", err)
			}
			return nil
		}
		return fmt.Errorf("%v: %w", a.Name(), err)
	}

	if !gctx.accessedWriter {
		return fmt.Errorf("did not produce output")
	}

	_ = gctx.w.Close()
	_ = f.Close()

	err = os.Rename(tmpp, p)
	if err != nil {
		return err
	}

	if a.GenerateManifest() {
		mp := filepath.Join(dir, a.ManifestFileName())

		b, err := json.Marshal(ArtifactManifest{
			Size: gctx.tracker.Written,
		})
		if err != nil {
			return err
		}

		err = os.WriteFile(mp, b, os.ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}
