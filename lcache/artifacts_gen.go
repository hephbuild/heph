package lcache

import (
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"io"
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

func (e *LocalCacheState) GenArtifacts(ctx context.Context, target graph.Targeter, arts []ArtifactWithProducer, compress bool) error {
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

		p := filepath.Join(dir, artifact.FileName())
		if shouldCompress {
			p = filepath.Join(dir, artifact.GzFileName())
		}

		err := GenArtifact(ctx, p, artifact, shouldCompress)
		if err != nil {
			return fmt.Errorf("genartifact %v: %w", artifact.Name(), err)
		}
	}

	return nil
}

type ArtifactGenContext struct {
	w              io.WriteCloser
	accessedWriter bool
}

func (g *ArtifactGenContext) Writer() io.Writer {
	g.accessedWriter = true
	return g.w
}

var ArtifactSkip = errors.New("skip artifact")

func GenArtifact(ctx context.Context, p string, a ArtifactWithProducer, compress bool) error {
	tmpp := xfs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	f, err := os.Create(tmpp)
	if err != nil {
		return err
	}
	defer f.Close()

	f, done := xio.ContextCloser(ctx, f)
	defer done()

	gctx := ArtifactGenContext{}
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

	return nil
}
