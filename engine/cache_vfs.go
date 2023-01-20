package engine

import (
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	log "heph/hlog"
	"os"
	"path/filepath"
	"time"
)

const inputHashName = "@input_hash"

func (e *Engine) vfsCachePath(target *Target, inputHash string) string {
	return filepath.Join(target.Package.FullName, target.Name, inputHash) + "/"
}

func (e *Engine) localCacheLocation(target *Target, inputHash string) (vfs.Location, error) {
	rel, err := filepath.Rel(e.LocalCache.Path(), e.cacheDirForHash(target, inputHash).Abs())
	if err != nil {
		return nil, err
	}

	return e.LocalCache.NewLocation(rel + "/")
}

func (e *Engine) remoteCacheLocation(loc vfs.Location, target *Target, inputHash string) (vfs.Location, error) {
	return loc.NewLocation(e.vfsCachePath(target, inputHash))
}

func (e *Engine) vfsExistsFile(from vfs.Location, path string) (bool, error) {
	sf, err := from.NewFile(path)
	if err != nil {
		return false, fmt.Errorf("NewFile: %w", err)
	}
	defer sf.Close()

	return sf.Exists()
}

func (e *Engine) vfsCopyFileIfNotExists(from, to vfs.Location, path string) error {
	tof, err := to.NewFile(path)
	if err != nil {
		return err
	}

	exists, err := tof.Exists()
	if err != nil {
		return err
	}

	if exists {
		log.Tracef("vfs copy %v to %v: exists", from.URI(), to.URI())
		return nil
	}

	return e.vfsCopyFile(from, to, path)
}
func (e *Engine) vfsCopyFile(from, to vfs.Location, path string) error {
	log.Tracef("vfs copy %v to %v", from.URI(), to.URI())

	start := time.Now()

	sf, err := from.NewFile(path)
	if err != nil {
		return fmt.Errorf("NewFile: %w", err)
	}
	defer sf.Close()

	ok, err := sf.Exists()
	if err != nil {
		return fmt.Errorf("Exists: %w", err)
	}

	log.Tracef("%v exists: %v", sf.URI(), ok)

	if !ok {
		return fmt.Errorf("vfs %v: %w", sf.URI(), os.ErrNotExist)
	}

	df, err := sf.CopyToLocation(to)
	if err != nil {
		return fmt.Errorf("CopyToLocation: %w", err)
	}

	defer df.Close()

	log.Debugf("vfs copy to %v took %v", to.URI(), time.Since(start).String())

	return nil
}

func (e *Engine) storeVfsCache(remote CacheConfig, target *Target) error {
	inputHash := e.hashInput(target)

	localRoot, err := e.localCacheLocation(target, inputHash)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(remote.Location, target, inputHash)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(localRoot, remoteRoot, versionFile)
	if err != nil {
		return err
	}

	for _, name := range target.OutWithSupport.Names() {
		err = e.vfsCopyFile(localRoot, remoteRoot, e.cacheOutTarName(name))
		if err != nil {
			return err
		}

		err = e.vfsCopyFile(localRoot, remoteRoot, e.cacheOutHashName(name))
		if err != nil {
			return err
		}
	}

	err = e.vfsCopyFile(localRoot, remoteRoot, inputHashFile)
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) getVfsCache(remoteRoot vfs.Location, cacheName string, target *Target, output string, onlyMeta bool) (bool, error) {
	what := target.FQN
	if output != "" {
		what += "|" + output
	}
	e.Status(TargetOutputStatus(target, output, fmt.Sprintf("Downloading meta from %v cache...", cacheName)))

	inputHash := e.hashInput(target)

	localRoot, err := e.localCacheLocation(target, inputHash)
	if err != nil {
		return false, err
	}

	remoteRoot, err = e.remoteCacheLocation(remoteRoot, target, inputHash)
	if err != nil {
		return false, err
	}

	if output == inputHashName {
		err = e.vfsCopyFileIfNotExists(remoteRoot, localRoot, inputHashFile)
		if err != nil {
			if errors.Is(err, os.ErrNotExist) {
				return false, nil
			}

			return false, err
		}

		return true, nil
	}

	err = e.vfsCopyFileIfNotExists(remoteRoot, localRoot, e.cacheOutHashName(output))
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}

		return false, err
	}

	if onlyMeta {
		e.Status(TargetOutputStatus(target, output, fmt.Sprintf("Downloading meta from %v cache...", cacheName)))

		ok, err := e.vfsExistsFile(remoteRoot, e.cacheOutTarName(output))
		if err != nil {
			return false, err
		}

		if !ok {
			return false, nil
		}
	} else {
		e.Status(TargetOutputStatus(target, output, fmt.Sprintf("Downloading from %v cache...", cacheName)))

		err = e.vfsCopyFileIfNotExists(remoteRoot, localRoot, e.cacheOutTarName(output))
		if err != nil {
			return false, err
		}
	}

	return true, nil
}
