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
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolwait"
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

// createDepsGenArtifacts returns the deps for each artifact by name
// For hashing to work properly:
//   - each hash must be preceded by its tar
//   - support_files must be first then the other artifacts
func (e *LocalCacheState) createDepsGenArtifacts(target *graph.Target, artsp []ArtifactWithProducer) (map[string]*worker.WaitGroup, map[string]*worker.WaitGroup) {
	arts := target.Artifacts

	deps := map[string]*worker.WaitGroup{}
	for _, artifact := range artsp {
		wg := &worker.WaitGroup{}
		deps[artifact.Name()] = wg
	}

	signals := map[string]*worker.WaitGroup{}
	for _, artifact := range artsp {
		wg := &worker.WaitGroup{}
		wg.AddSem()
		signals[artifact.Name()] = wg
	}

	var support *worker.WaitGroup
	if target.HasSupportFiles {
		support = signals[arts.OutHash(specs.SupportFilesOutput).Name()]
	}

	for _, output := range target.OutWithSupport.Names() {
		tname := arts.OutTar(output).Name()
		hname := arts.OutHash(output).Name()

		deps[hname].AddChild(signals[tname])

		if support != nil && output != specs.SupportFilesOutput {
			deps[tname].AddChild(support)
		}
	}

	meta := []string{arts.InputHash.Name(), arts.Manifest.Name()}

	allButMeta := &worker.WaitGroup{}
	for _, art := range artsp {
		if !ads.Contains(meta, art.Name()) {
			allButMeta.AddChild(signals[art.Name()])
		}
	}

	for _, name := range meta {
		deps[name].AddChild(allButMeta)
	}

	return deps, signals
}

func (e *LocalCacheState) ScheduleGenArtifacts(ctx context.Context, gtarget graph.Targeter, arts []ArtifactWithProducer, compress bool) (_ *worker.WaitGroup, rerr error) {
	target := gtarget.GraphTarget()

	unlock, err := e.LockArtifacts(ctx, target, artifacts.ToSlice(arts))
	if err != nil {
		return nil, err
	}
	defer func() {
		if rerr != nil {
			unlock()
		}
	}()

	dir := e.cacheDir(target).Abs()

	err = os.RemoveAll(dir)
	if err != nil {
		return nil, err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return nil, err
	}

	allDeps := &worker.WaitGroup{}

	deps, signals := e.createDepsGenArtifacts(target, arts)

	for _, artifact := range arts {
		artifact := artifact

		shouldCompress := artifact.Compressible() && compress

		j := e.Pool.Schedule(ctx, &worker.Job{
			Name: fmt.Sprintf("store %v|%v", target.Addr, artifact.Name()),
			Deps: deps[artifact.Name()],
			Hook: worker.StageHook{
				OnEnd: func(job *worker.Job) context.Context {
					signals[artifact.Name()].DoneSem()
					return nil
				},
			},
			Do: func(w *worker.Worker, ctx context.Context) error {
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
		})
		allDeps.Add(j)
	}

	go func() {
		<-allDeps.Done()
		unlock()
	}()

	if fgDeps := poolwait.ForegroundWaitGroup(ctx); fgDeps != nil {
		fgDeps.AddChild(allDeps)
	}

	return allDeps, nil
}

// Deprecated: use ScheduleGenArtifacts instead
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
