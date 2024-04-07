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
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/worker2"
	"io"
	"math"
	"math/rand"
	"os"
	"path/filepath"
	"time"
)

func (e *LocalCacheState) LockArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (func(), error) {
	_, unlock, err := e.lockArtifact(ctx, target, artifact, false)
	return unlock, err
}

func (e *LocalCacheState) lockArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact, try bool) (bool, func(), error) {
	starget := target.Spec()
	l := e.Metas.Find(target).artifactLocks[artifact.Name()]

	unlock := func() {
		err := l.Unlock()
		if err != nil {
			log.Errorf("unlock %v %v: %v", starget.Addr, artifact.Name(), err)
		}
	}

	if try {
		ok, err := l.TryLock(ctx)
		if !ok || err != nil {
			return false, nil, err
		}
		return true, unlock, nil
	} else {
		err := l.Lock(ctx)
		if err != nil {
			return false, nil, fmt.Errorf("lock %v %v: %w", starget.Addr, artifact.Name(), err)
		}

		return true, unlock, nil
	}
}

func (e *LocalCacheState) tryLockArtifacts(ctx context.Context, target graph.Targeter, artifacts []artifacts.Artifact) (bool, func(), error) {
	// Prevents concurrent multiple locks attempts
	l := e.Metas.Find(target).allArtifactsLock
	err := l.Lock(ctx)
	if err != nil {
		return false, nil, err
	}
	defer func() {
		_ = l.Unlock()
	}()

	unlockAll := &locks.Multi{}

	for _, artifact := range artifacts {
		ok, unlock, err := e.lockArtifact(ctx, target, artifact, true)
		if err != nil || !ok {
			_ = unlockAll.Unlock()
			return false, nil, err
		}

		unlockAll.AddFunc(func() error {
			unlock()
			return nil
		})
	}

	return true, func() {
		err := unlockAll.Unlock()
		if err != nil {
			log.Errorf("artifacts: unlock: %v", err)
		}
	}, nil
}

func (e *LocalCacheState) LockAllArtifacts(ctx context.Context, target graph.Targeter) (func(), error) {
	l := e.Metas.Find(target).allArtifactsLock
	err := l.Lock(ctx)
	if err != nil {
		return nil, err
	}

	return func() {
		_ = l.Unlock()
	}, nil
}

func (e *LocalCacheState) LockArtifacts(ctx context.Context, target graph.Targeter, artifacts []artifacts.Artifact) (func(), error) {
	for i := 1; ; i++ {
		ok, unlock, err := e.tryLockArtifacts(ctx, target, artifacts)
		if err != nil {
			return nil, err
		}

		// We failed to acquire one lock, retry, with some jitter
		if !ok {
			delay := time.Duration(i*i*10) * time.Millisecond
			if delay > time.Second {
				delay = time.Second
			}
			delay += time.Duration(rand.Int63n(int64(delay / 3)))

			select {
			case <-time.After(delay):
				continue
			case <-ctx.Done():
				return nil, ctx.Err()
			}
		}

		return unlock, nil
	}
}

// createDepsGenArtifacts returns the deps for each artifact by name
// For hashing to work properly:
//   - each hash must be preceded by its tar
//   - support_files must be first then the other artifacts
func (e *LocalCacheState) createDepsGenArtifacts(target *graph.Target, artsp []ArtifactWithProducer) (map[string]*worker2.Group, map[string]*worker2.Sem) {
	arts := target.Artifacts

	deps := map[string]*worker2.Group{}
	for _, artifact := range artsp {
		wg := &worker2.Group{}
		deps[artifact.Name()] = wg
	}

	signals := map[string]*worker2.Sem{}
	for _, artifact := range artsp {
		wg := worker2.NewSemDep()
		wg.AddSem(1)
		signals[artifact.Name()] = wg
	}

	var support worker2.Dep
	if target.HasSupportFiles {
		support = signals[arts.OutHash(specs.SupportFilesOutput).Name()]
	}

	for _, output := range target.OutWithSupport.Names() {
		tname := arts.OutTar(output).Name()
		hname := arts.OutHash(output).Name()

		deps[hname].AddDep(signals[tname])

		if support != nil && output != specs.SupportFilesOutput {
			deps[tname].AddDep(support)
		}
	}

	meta := []string{arts.InputHash.Name(), arts.Manifest.Name()}

	allButMeta := &worker2.Group{}
	for _, art := range artsp {
		if !ads.Contains(meta, art.Name()) {
			allButMeta.AddDep(signals[art.Name()])
		}
	}

	for _, name := range meta {
		deps[name].AddDep(allButMeta)
	}

	return deps, signals
}

func (e *LocalCacheState) ScheduleGenArtifacts(ctx context.Context, gtarget graph.Targeter, arts []ArtifactWithProducer, compress bool) (*worker2.Group, error) {
	target := gtarget.GraphTarget()

	dirp, err := e.cacheDir(target)
	if err != nil {
		return nil, err
	}

	dir := dirp.Abs()

	err = os.RemoveAll(dir)
	if err != nil {
		return nil, err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return nil, err
	}

	allDeps := &worker2.Group{}

	deps, signals := e.createDepsGenArtifacts(target, arts)

	for _, artifact := range arts {
		artifact := artifact

		shouldCompress := artifact.Compressible() && compress

		j := e.Pool.Schedule(worker2.NewAction(worker2.ActionConfig{
			Ctx:  ctx,
			Name: fmt.Sprintf("store %v|%v", target.Addr, artifact.Name()),
			Deps: []worker2.Dep{deps[artifact.Name()]},
			Hooks: []worker2.Hook{
				worker2.StageHook{
					OnEnd: func(worker2.Dep) context.Context {
						signals[artifact.Name()].DoneSem()
						return nil
					},
				}.Hook(),
			},
			Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
				err := GenArtifact(ctx, dir, artifact, shouldCompress, func(percent float64) {
					var s string
					if target.Cache.Enabled {
						s = xmath.FormatPercent("Caching [P]...", percent)
					} else if len(target.Artifacts.Out) > 0 {
						s = xmath.FormatPercent("Storing [P]...", percent)
					}

					if s != "" {
						status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.Name(), s))
					}
				})
				if err != nil {
					return fmt.Errorf("genartifact %v: %w", artifact.Name(), err)
				}

				return nil
			},
		}))

		allDeps.AddDep(j)
	}

	e.Pool.Schedule(allDeps)

	return allDeps, nil
}

// Deprecated: use ScheduleGenArtifacts instead
func (e *LocalCacheState) GenArtifacts(ctx context.Context, gtarget graph.Targeter, arts []ArtifactWithProducer, compress bool) error {
	target := gtarget.GraphTarget()

	dirp, err := e.cacheDir(target)
	if err != nil {
		return err
	}

	dir := dirp.Abs()

	err = os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	for i, artifact := range arts {
		shouldCompress := artifact.Compressible() && compress

		err := GenArtifact(ctx, dir, artifact, shouldCompress, func(percent float64) {
			var s string
			if target.Cache.Enabled {
				s = xmath.FormatPercent("Caching [P]...", percent)
			} else if len(target.Artifacts.Out) > 0 {
				s = xmath.FormatPercent("Storing [P]...", percent)
			}

			if s != "" {
				status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.Name(),
					fmt.Sprintf("%v/%v %v", i+1, len(arts), s)),
				)
			}
		})
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
	progress       func(percent float64)
}

func (g *ArtifactGenContext) EstimatedWriteSize(size int64) {
	progress := g.progress
	if progress == nil {
		return
	}

	g.tracker.OnWrite = func(written int64) {
		progress(math.Round(xmath.Percent(written, size)))
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

func GenArtifact(ctx context.Context, dir string, a ArtifactWithProducer, compress bool, progress func(percent float64)) error {
	if progress != nil {
		progress(-1)
	}

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
		w:        f,
	}

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
		return err
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
