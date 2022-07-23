package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"os"
	"path/filepath"
	"time"
)

func (e *Engine) vfsCachePath(target *Target, inputHash string) string {
	return filepath.Join(target.Package.FullName, target.Name, inputHash) + "/"
}

func (e *Engine) localCacheLocation(target *Target, inputHash string) (vfs.Location, error) {
	return e.LocalCache.NewLocation(e.vfsCachePath(target, inputHash))
}

func (e *Engine) remoteCacheLocation(loc vfs.Location, target *Target, inputHash string) (vfs.Location, error) {
	return loc.NewLocation(e.vfsCachePath(target, inputHash))
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
		return fmt.Errorf("%v: %w", sf.URI(), os.ErrNotExist)
	}

	df, err := sf.CopyToLocation(to)
	if err != nil {
		return fmt.Errorf("CopyToLocation: %w", err)
	}
	defer df.Close()

	log.Tracef("vfs copy to %v took %v", to.URI(), time.Since(start).String())

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

	err = e.vfsCopyFile(localRoot, remoteRoot, outputTarFile)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(localRoot, remoteRoot, inputHashFile)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(localRoot, remoteRoot, outputHashFile)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) getVfsCache(remoteRoot vfs.Location, target *Target) (bool, error) {
	inputHash := e.hashInput(target)

	localRoot, err := e.localCacheLocation(target, inputHash)
	if err != nil {
		return false, err
	}

	remoteRoot, err = e.remoteCacheLocation(remoteRoot, target, inputHash)
	if err != nil {
		return false, err
	}

	err = e.vfsCopyFile(remoteRoot, localRoot, inputHashFile)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}

		return false, err
	}

	err = e.vfsCopyFile(remoteRoot, localRoot, outputHashFile)
	if err != nil {
		return false, err
	}

	err = e.vfsCopyFile(remoteRoot, localRoot, outputTarFile)
	if err != nil {
		return false, err
	}

	dir := e.cacheDir(target, inputHash)

	err = utils.Untar(context.Background(), filepath.Join(dir, outputTarFile), filepath.Join(dir, outputDir))
	if err != nil {
		return false, err
	}

	return true, nil
}
