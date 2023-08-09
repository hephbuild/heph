package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
)

func (e *Engine) tmpRoot(elem ...string) xfs.Path {
	return e.Root.Home.Join("tmp").Join(elem...)
}

func (e *Engine) tmpTargetRoot(target specs.Specer) xfs.Path {
	spec := target.Spec()
	return e.Root.Home.Join("tmp", spec.Package.Path, "__target_"+spec.Name)
}

func (e *Engine) lockPath(target specs.Specer, resource string) string {
	return e.tmpTargetRoot(target).Join(resource + ".lock").Abs()
}

func (e *Engine) toolAbsPath(tt graph.TargetTool) string {
	return tt.File.WithRoot(e.LocalCache.Metas.Find(tt.Target).OutExpansionRoot().Abs()).Abs()
}

func (e *Engine) WriteableCaches(ctx context.Context, starget specs.Specer) ([]graph.CacheConfig, error) {
	target := starget.Spec()

	if !target.Cache.Enabled || e.DisableNamedCacheWrite {
		return nil, nil
	}

	wcs := make([]graph.CacheConfig, 0)
	for _, cache := range e.Config.Caches {
		if !cache.Write {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		wcs = append(wcs, cache)
	}

	if len(wcs) == 0 {
		return nil, nil
	}

	orderedCaches, err := e.OrderedCaches(ctx)
	if err != nil {
		return nil, err
	}

	// Reset and re-add in order
	wcs = nil

	for _, cache := range orderedCaches {
		if !cache.Write {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		wcs = append(wcs, cache)
	}

	return wcs, nil
}

func (e *Engine) RunWithSpan(ctx context.Context, rr targetrun.Request, iocfg sandbox.IOConfig) (rerr error) {
	ctx, rspan := e.Observability.SpanRun(ctx, rr.Target)
	defer rspan.EndError(rerr)

	return e.Run(ctx, rr, iocfg)
}

func (e *Engine) Run(ctx context.Context, rr targetrun.Request, iocfg sandbox.IOConfig) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	gtarget := rr.Target

	if gtarget.Cache.Enabled && !rr.Shell && !rr.NoCache {
		if len(rr.Args) > 0 {
			return fmt.Errorf("args are not supported with cache")
		}

		cached, err := e.pullOrGetCacheAndPost(ctx, gtarget, gtarget.OutWithSupport.Names(), true, false)
		if err != nil {
			return err
		}

		if cached {
			return nil
		}
	}

	done := log.TraceTiming("run " + gtarget.Addr)
	defer done()

	writeableCaches, err := e.WriteableCaches(ctx, gtarget)
	if err != nil {
		return fmt.Errorf("wcs: %w", err)
	}

	rr.Compress = len(writeableCaches) > 0

	target, err := e.Runner.Run(ctx, rr, iocfg)
	if err != nil {
		return targetrun.WrapTargetFailed(err, gtarget)
	}

	if target == nil {
		log.Debugf("target is nil after run")
		return nil
	}

	if len(writeableCaches) > 0 {
		for _, cache := range writeableCaches {
			j := e.scheduleStoreExternalCache(ctx, target.Target, cache)

			if poolDeps := ForegroundWaitGroup(ctx); poolDeps != nil {
				poolDeps.Add(j)
			}
		}
	}

	err = e.postRunOrWarm(ctx, gtarget, target.Out.Names())
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) postRunOrWarm(ctx context.Context, target *graph.Target, outputs []string) error {
	ltarget := e.LocalCache.Metas.Find(target)

	hash, err := e.LocalCache.HashInput(target)
	if err != nil {
		return err
	}

	_, err = e.LocalCache.Expand(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("expand: %w", err)
	}

	err = e.codegenLink(ctx, ltarget)
	if err != nil {
		return fmt.Errorf("codegenlink: %w", err)
	}

	err = e.LocalCache.LinkLatestCache(target, hash)
	if err != nil {
		return fmt.Errorf("linklatest: %w", err)
	}

	err = e.gc(ctx, target)
	if err != nil {
		log.Errorf("gc %v: %v", target.Addr, err)
	}

	return nil
}

func (e *Engine) gc(ctx context.Context, target *graph.Target) error {
	if target.Cache.Enabled && e.Config.Engine.GC {
		status.Emit(ctx, tgt.TargetStatus(target, "GC..."))

		err := e.LocalCache.GCTargets([]*graph.Target{target}, nil, false)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *Engine) codegenLink(ctx context.Context, target *lcache.Target) error {
	if target.Codegen == "" {
		return nil
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Linking output..."))

	for name, paths := range target.Out.Named() {
		if err := ctx.Err(); err != nil {
			return err
		}

		switch target.Codegen {
		case specs.CodegenCopy, specs.CodegenCopyNoExclude:
			tarf, err := e.LocalCache.UncompressedReaderFromArtifact(target.Artifacts.OutTar(name), target)
			if err != nil {
				return err
			}

			err = tar.UntarContext(ctx, tarf, e.Root.Root.Abs(), tar.UntarOptions{})
			_ = tarf.Close()
			if err != nil {
				return err
			}
		case specs.CodegenLink:
			for _, path := range paths {
				from := path.WithRoot(target.OutExpansionRoot().Abs()).Abs()
				to := path.WithRoot(e.Root.Root.Abs()).Abs()

				info, err := os.Lstat(to)
				if err != nil && !errors.Is(err, os.ErrNotExist) {
					return err
				}
				exists := err == nil

				if exists {
					isLink := info.Mode().Type() == os.ModeSymlink

					if !isLink {
						log.Warnf("linking codegen: %v already exists", to)
						continue
					}

					err := os.Remove(to)
					if err != nil {
						return err
					}
				}

				err = xfs.CreateParentDir(to)
				if err != nil {
					return err
				}

				err = os.Symlink(from, to)
				if err != nil {
					return err
				}
			}
		}
	}

	return nil
}
