package engine

import (
	"context"
	"fmt"
	log "heph/hlog"
	"heph/packages"
	"heph/platform"
	"heph/upgrade"
	"os"
	"time"
)

func (e *Engine) Init(ctx context.Context) error {
	cwd, err := os.Getwd()
	if err != nil {
		return err
	}

	e.Cwd = cwd

	configStartTime := time.Now()
	err = e.parseConfigs()
	if err != nil {
		return err
	}
	log.Debugf("ParseConfigs took %v", time.Since(configStartTime))

	err = upgrade.CheckAndUpdate(ctx, e.Config.Config)
	if err != nil {
		return fmt.Errorf("upgrade: %w", err)
	}

	return nil
}

func (e *Engine) Parse(ctx context.Context) error {
	for name, cfg := range e.Config.BuildFiles.Roots {
		err := e.runRootBuildFiles(ctx, name, cfg)
		if err != nil {
			return fmt.Errorf("root %v: %w", name, err)
		}
	}

	runStartTime := time.Now()
	err := e.runBuildFiles(e.Root.Abs(), func(dir string) *packages.Package {
		return e.createPkg(dir)
	})
	if err != nil {
		return err
	}
	log.Debugf("RunBuildFiles took %v", time.Since(runStartTime))

	e.Targets.Sort()

	processStartTime := time.Now()
	for _, target := range e.Targets.Slice() {
		err := e.processTarget(target)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}
	log.Debugf("ProcessTargets took %v", time.Since(processStartTime))

	if e.Config.Engine.InstallTools {
		err = e.InstallTools(ctx)
		if err != nil {
			return err
		}
	}

	e.autocompleteHash, err = e.computeAutocompleteHash()
	if err != nil {
		return err
	}

	e.PlatformProviders = []PlatformProvider{}
	for _, p := range e.Config.OrderedPlatforms() {
		provider, err := platform.GetProvider(p.Provider, p.Name, p.Options)
		if err != nil {
			log.Warnf("provider: %v: %v", p.Name, err)
			continue
		}

		e.PlatformProviders = append(e.PlatformProviders, PlatformProvider{
			Platform: p,
			Provider: provider,
		})
	}

	return nil
}
