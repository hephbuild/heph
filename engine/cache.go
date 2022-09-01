package engine

import (
	"context"
	"errors"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"heph/vfssimple"
	"os"
)

func (e *Engine) cacheDir(target *Target, inputHash string) Path {
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

	err = utils.WriteFileSync(dir.Join(versionFile).Abs(), []byte("1"), os.ModePerm)
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

		files := make([]utils.TarFile, 0)
		for _, file := range cache {
			if err := ctx.Err(); err != nil {
				return err
			}

			err := utils.Cp(file.Abs(), outFilesDir.Join(file.RelRoot()).Abs())
			if err != nil {
				return err
			}

			files = append(files, utils.TarFile{
				From: file.Abs(),
				To:   file.RelRoot(),
			})
		}

		log.Tracef("Creating archive %v", target.FQN)

		err = utils.Tar(ctx, files, e.targetOutputTarFile(target, inputHash))
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

	err = utils.WriteFileSync(dir.Join(outputHashFile).Abs(), []byte(outputHash), os.ModePerm)
	if err != nil {
		return err
	}

	err = utils.WriteFileSync(dir.Join(inputHashFile).Abs(), []byte(inputHash), os.ModePerm)
	if err != nil {
		return err
	}

	latestDir := e.cacheDir(target, "latest")

	err = os.RemoveAll(latestDir.Abs())
	if err != nil {
		return err
	}

	err = os.Symlink(dir.Abs(), latestDir.Abs())
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) getCache(target *Target) (*Path, error) {
	p, err := e.getLocalCache(target)
	if err != nil {
		return nil, err
	}

	if p != nil {
		log.Debugf("%v local cache hit", target.FQN)
		return p, nil
	}

	for _, cache := range e.Config.Cache {
		if !cache.Read {
			continue
		}

		loc, err := vfssimple.NewLocation(cache.URI)
		if err != nil {
			return nil, err
		}

		ok, err := e.getVfsCache(loc, target)
		if err != nil {
			log.Errorf("cache %v: %v", cache.Name, err)
			continue
		}

		if ok {
			log.Debugf("%v %v cache hit", target.FQN, cache.Name)
			return e.getLocalCache(target)
		} else {
			log.Debugf("%v %v cache miss", target.FQN, cache.Name)
		}
	}

	return nil, nil
}

func (e *Engine) getLocalCache(target *Target) (*Path, error) {
	hash := e.hashInput(target)

	dir := e.cacheDir(target, hash)

	cacheHashb, err := os.ReadFile(dir.Join(inputHashFile).Abs())
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}

		return nil, err
	}
	cacheHash := string(cacheHashb)

	log.Tracef("Cache %v: %v %v", target.FQN, hash, cacheHash)

	if cacheHash != hash {
		return nil, nil
	}

	outDir := dir.Join(outputDir)

	latestDir := e.cacheDir(target, "latest")

	err = os.RemoveAll(latestDir.Abs())
	if err != nil {
		return nil, err
	}

	err = os.Symlink(dir.Abs(), latestDir.Abs())
	if err != nil {
		return nil, err
	}

	return &outDir, nil
}
