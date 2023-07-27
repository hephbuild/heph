package lcache

import (
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/xfs"
	"io"
	"os"
	"path/filepath"
)

func (e *LocalCacheState) LockArtifact(ctx context.Context, starget specs.Specer, artifact artifacts.Artifact) (func() error, error) {
	target := starget.Spec()
	l := e.TargetMetas.Find(target).cacheLocks[artifact.Name()]
	err := l.Lock(ctx)
	if err != nil {
		return nil, fmt.Errorf("lock %v %v: %w", target.FQN, artifact.Name(), err)
	}

	return func() error {
		err := l.Unlock()
		if err != nil {
			return fmt.Errorf("unlock %v %v: %w", target.FQN, artifact.Name(), err)
		}

		return nil
	}, nil
}

func (e *LocalCacheState) LockArtifacts(ctx context.Context, starget specs.Specer, allArtifacts []ArtifactWithProducer) (func(), error) {
	target := starget.Spec()

	unlockers := make([]func() error, 0, len(allArtifacts))

	unlocker := func() {
		for _, unlock := range unlockers {
			err := unlock()
			if err != nil {
				log.Errorf("unlock %v: %v", target.FQN, err)
			}
		}
	}

	for _, artifact := range allArtifacts {
		unlock, err := e.LockArtifact(ctx, target, artifact)
		if err != nil {
			unlocker()
			return nil, err
		}
		unlockers = append(unlockers, unlock)
	}

	return unlocker, nil
}

func (e *LocalCacheState) GenArtifacts(ctx context.Context, dir string, target specs.Specer, allArtifacts []ArtifactWithProducer, compress bool) error {
	unlock, err := e.LockArtifacts(ctx, target, allArtifacts)
	if err != nil {
		return err
	}
	defer unlock()

	for _, artifact := range allArtifacts {
		_, err := GenArtifact(ctx, dir, artifact, compress)
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

func GenArtifact(ctx context.Context, dir string, a ArtifactWithProducer, compress bool) (string, error) {
	shouldCompress := a.Compressible() && compress

	p := filepath.Join(dir, a.FileName())
	if shouldCompress {
		p = filepath.Join(dir, a.GzFileName())
	}

	tmpp := xfs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	f, err := os.Create(tmpp)
	if err != nil {
		return "", err
	}
	defer f.Close()

	go func() {
		<-ctx.Done()
		_ = f.Close()
	}()

	gctx := ArtifactGenContext{}
	gctx.w = f

	if shouldCompress {
		gw := gzip.NewWriter(f)
		defer gw.Close()

		gctx.w = gw
	}

	err = a.Gen(ctx, &gctx)
	if err != nil {
		if errors.Is(err, ArtifactSkip) {
			if a.GenRequired() {
				return "", fmt.Errorf("%v is required, but returned: %w", a.Name(), err)
			}
			return "", nil
		}
		return "", fmt.Errorf("%v: %w", a.Name(), err)
	}

	if !gctx.accessedWriter {
		return "", fmt.Errorf("%v did not produce output", a.Name())
	}

	_ = gctx.w.Close()
	_ = f.Close()

	err = os.Rename(tmpp, p)
	if err != nil {
		return "", err
	}

	return p, nil
}
