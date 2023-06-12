package engine

import (
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/mds"
	"github.com/hephbuild/heph/utils/xfs"
	"io"
	"os"
	"path/filepath"
)

var ArtifactSkip = errors.New("skip artifact")

type ArtifactGenContext struct {
	w              io.WriteCloser
	accessedWriter bool
}

func (g *ArtifactGenContext) Writer() io.Writer {
	g.accessedWriter = true
	return g.w
}

type ArtifactProducer interface {
	Gen(ctx context.Context, gctx *ArtifactGenContext) error
}

type ArtifactWithProducer interface {
	artifacts.Artifact
	ArtifactProducer
}

type artifactProducer struct {
	artifacts.Artifact
	ArtifactProducer
}

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

// orderedArtifactProducers returns artifact in hashing order
// For hashing to work properly, support_files tar must go first, then support_files hash
// then the other artifacts
func (e *LocalCacheState) orderedArtifactProducers(t *Target, outRoot, logFilePath string) []ArtifactWithProducer {
	arts := t.Artifacts

	all := make([]ArtifactWithProducer, 0, len(arts.Out)+2)
	all = append(all, artifactProducer{arts.Log, logArtifact{
		LogFilePath: logFilePath,
	}})
	names := mds.Keys(arts.Out)
	names = targetspec.SortOutputsForHashing(names)
	for _, name := range names {
		a := arts.Out[name]
		all = append(all, artifactProducer{a.Tar(), outTarArtifact{
			Target:  t,
			Output:  name,
			OutRoot: outRoot,
		}})
		all = append(all, artifactProducer{a.Hash(), hashOutputArtifact{
			LocalState: e,
			Target:     t,
			Output:     name,
		}})
	}
	all = append(all, artifactProducer{arts.Manifest, manifestArtifact{
		LocalState: e,
		Target:     t,
	}})
	all = append(all, artifactProducer{arts.InputHash, hashInputArtifact{
		LocalState: e,
		Target:     t,
	}})
	return all
}

func (e *LocalCacheState) LockArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact) (func() error, error) {
	l := target.cacheLocks[artifact.Name()]
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

func (e *LocalCacheState) LockArtifacts(ctx context.Context, target *Target, allArtifacts []ArtifactWithProducer) (func(), error) {
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

func (e *LocalCacheState) GenArtifacts(ctx context.Context, dir string, target *Target, allArtifacts []ArtifactWithProducer, compress bool) error {
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
