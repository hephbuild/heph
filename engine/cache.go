package engine

import (
	"context"
	"errors"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"heph/vfssimple"
	"os"
	"path/filepath"
)

func (e *Engine) cacheDir(target *Target, inputHash string) string {
	return filepath.Join(e.HomeDir, "cache", target.Package.FullName, target.Name, inputHash)
}

const versionFile = "version"
const inputHashFile = "hash_input"
const outputHashFile = "hash_output"
const outputTarFile = "output.tar.gz"
const outputDir = "_output"

func (e *Engine) storeCache(ctx context.Context, target *Target) error {
	inputHash := e.hashInput(target)

	log.Tracef("Store Cache %v %v", target.FQN, inputHash)

	// TODO: store at `hash` and create a link to `latest`
	dir := e.cacheDir(target, inputHash)

	err := os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	err = WriteFileSync(filepath.Join(dir, versionFile), []byte("1"), os.ModePerm)
	if err != nil {
		return err
	}

	outFilesDir := filepath.Join(dir, outputDir)

	err = os.MkdirAll(outFilesDir, os.ModePerm)
	if err != nil {
		return err
	}

	if len(target.ActualFilesOut()) > 0 {
		cache := target.CachedFilesInOutRoot()

		log.Tracef("Copying to cache %v", target.FQN)

		files := make([]utils.TarFile, 0)
		for _, file := range cache {
			err := utils.Cp(file.Abs(), filepath.Join(outFilesDir, file.RelRoot()))
			if err != nil {
				return err
			}

			files = append(files, utils.TarFile{
				From: file.Abs(),
				To:   file.RelRoot(),
			})
		}

		log.Tracef("Creating archive %v", target.FQN)

		err = utils.Tar(ctx, files, filepath.Join(dir, outputTarFile))
		if err != nil {
			return err
		}
	}

	rel, err := filepath.Rel(e.Root, outFilesDir)
	if err != nil {
		return err
	}

	target.OutRoot = &Path{
		Abs:     outFilesDir,
		RelRoot: rel,
	}

	for i, file := range target.actualFilesOut {
		target.actualFilesOut[i] = file.WithRoot(target.OutRoot.Abs)
	}

	outputHash := e.hashOutput(target)

	err = WriteFileSync(filepath.Join(dir, outputHashFile), []byte(outputHash), os.ModePerm)
	if err != nil {
		return err
	}

	err = WriteFileSync(filepath.Join(dir, inputHashFile), []byte(inputHash), os.ModePerm)
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

	cacheHashb, err := os.ReadFile(filepath.Join(dir, inputHashFile))
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

	outDir := filepath.Join(dir, outputDir)

	rel, err := filepath.Rel(e.Root, outDir)
	if err != nil {
		return nil, err
	}

	return &Path{
		Abs:     outDir,
		RelRoot: rel,
	}, nil
}

func WriteFileSync(name string, data []byte, perm os.FileMode) error {
	f, err := os.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, perm)
	if err != nil {
		return err
	}
	_, err = f.Write(data)
	if err1 := f.Sync(); err1 != nil && err == nil {
		err = err1
	}
	if err1 := f.Close(); err1 != nil && err == nil {
		err = err1
	}
	return err
}
