package engine

import (
	"context"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils/fs"
	"heph/utils/tar"
	"heph/vfssimple"
	"os"
)

func (e *Engine) cacheDir(target *Target, inputHash string) fs.Path {
	return e.HomeDir.Join("cache", target.Package.FullName, "__target_"+target.Name, inputHash)
}

func (e *Engine) targetOutputTarFile(target *Target, inputHash string) string {
	return e.cacheDir(target, inputHash).Join(outputTarFile).Abs()
}

const versionFile = "version"
const inputHashFile = "hash_input"
const outputHashFile = "hash_output"
const outputTarFile = "output.tar.gz"
const outputDir = "_output"

func (e *Engine) storeCache(ctx context.Context, target *Target) error {
	inputHash := e.hashInput(target)

	log.Tracef("Store Cache %v %v", target.FQN, inputHash)

	dir := e.cacheDir(target, inputHash)

	err := os.RemoveAll(dir.Abs())
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir.Abs(), os.ModePerm)
	if err != nil {
		return err
	}

	err = fs.WriteFileSync(dir.Join(versionFile).Abs(), []byte("1"), os.ModePerm)
	if err != nil {
		return err
	}

	outFilesDir := dir.Join(outputDir)

	err = os.MkdirAll(outFilesDir.Abs(), os.ModePerm)
	if err != nil {
		return err
	}

	if cache := target.ActualCachedFiles(); len(cache) > 0 {
		log.Tracef("Copying to cache %v", target.FQN)

		files := make([]tar.TarFile, 0)
		for _, file := range cache {
			if err := ctx.Err(); err != nil {
				return err
			}

			err := fs.Cp(file.Abs(), outFilesDir.Join(file.RelRoot()).Abs())
			if err != nil {
				return err
			}

			files = append(files, tar.TarFile{
				From: file.Abs(),
				To:   file.RelRoot(),
			})
		}

		log.Tracef("Creating archive %v", target.FQN)

		err = tar.Tar(ctx, files, e.targetOutputTarFile(target, inputHash))
		if err != nil {
			return err
		}
	} else {
		log.Tracef("%v output no files, skipping archive", target.FQN)
	}

	target.OutRoot = &outFilesDir

	target.actualFilesOut = target.actualFilesOut.WithRoot(target.OutRoot.Abs())
	target.actualcachedFiles = target.actualcachedFiles.WithRoot(target.OutRoot.Abs())

	outputHash := e.hashOutput(target, "")

	err = fs.WriteFileSync(dir.Join(outputHashFile).Abs(), []byte(outputHash), os.ModePerm)
	if err != nil {
		return err
	}

	err = fs.WriteFileSync(dir.Join(inputHashFile).Abs(), []byte(inputHash), os.ModePerm)
	if err != nil {
		return err
	}

	err = e.linkLatestCache(target, dir.Abs())
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) getCache(target *Target, onlyMeta bool) (bool, *fs.Path, error) {
	ok, p, err := e.getLocalCache(target, onlyMeta)
	if err != nil {
		return false, nil, err
	}

	if ok {
		e.Status(fmt.Sprintf("Using local %v cache...", target.FQN))
		return true, p, nil
	}

	if e.DisableNamedCache {
		return false, nil, nil
	}

	for _, cache := range e.Config.Cache {
		if !cache.Read {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		loc, err := vfssimple.NewLocation(cache.URI)
		if err != nil {
			return false, nil, err
		}

		ok, err := e.getVfsCache(loc, cache.Name, target, onlyMeta)
		if err != nil {
			log.Errorf("cache %v: %v", cache.Name, err)
			continue
		}

		if ok {
			log.Debugf("%v %v cache hit", target.FQN, cache.Name)
			return e.getLocalCache(target, onlyMeta)
		} else {
			log.Debugf("%v %v cache miss", target.FQN, cache.Name)
		}
	}

	return false, nil, nil
}

func (e *Engine) getLocalCache(target *Target, onlyMeta bool) (bool, *fs.Path, error) {
	hash := e.hashInput(target)

	dir := e.cacheDir(target, hash)

	cacheHashb, err := os.ReadFile(dir.Join(inputHashFile).Abs())
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil, nil
		}

		return false, nil, err
	}
	cacheHash := string(cacheHashb)

	log.Tracef("Cache %v: %v %v", target.FQN, hash, cacheHash)

	if cacheHash != hash {
		return false, nil, nil
	}

	if !fs.PathExists(dir.Join(outputHashFile).Abs()) {
		return false, nil, nil
	}

	if onlyMeta {
		return true, nil, nil
	}

	outDir := dir.Join(outputDir)

	if !fs.PathExists(outDir.Abs()) {
		return false, nil, nil
	}

	err = e.linkLatestCache(target, dir.Abs())
	if err != nil {
		return false, nil, err
	}

	return true, &outDir, nil
}

func (e *Engine) linkLatestCache(target *Target, from string) error {
	latestDir := e.cacheDir(target, "latest")

	err := os.RemoveAll(latestDir.Abs())
	if err != nil {
		return err
	}

	err = os.Symlink(from, latestDir.Abs())
	if err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}

	return nil
}
