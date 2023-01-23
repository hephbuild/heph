package engine

import (
	"context"
	"errors"
	"fmt"
	log "heph/hlog"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/fs"
	"heph/utils/tar"
	"heph/vfssimple"
	"os"
	"time"
)

func (e *Engine) cacheDir(target *Target) fs.Path {
	return e.cacheDirForHash(target, e.hashInput(target))
}

var SOMEID = fmt.Sprintf("%v%v", os.Getpid(), time.Now().Nanosecond())

func (e *Engine) cacheDirForHash(target *Target, inputHash string) fs.Path {
	folder := "__target_" + target.Name
	if !target.Cache.Enabled {
		folder = "__target_tmp_" + SOMEID + "_" + target.Name
	}

	p := e.HomeDir.Join("cache", target.Package.FullName, folder)

	if !target.Cache.Enabled {
		e.RegisterRemove(p.Abs())
	}

	return p.Join(inputHash)
}

func (e *Engine) cacheOutTarName(name string) string {
	return "out_" + name + ".tar.gz"
}

func (e *Engine) cacheOutHashName(name string) string {
	return "hash_out_" + name
}

func (e *Engine) targetOutputTarFile(target *Target, name string) string {
	return e.cacheDir(target).Join(e.cacheOutTarName(name)).Abs()
}

func (e *Engine) targetOutputTarFileForHash(target *Target, hash, name string) string {
	return e.cacheDirForHash(target, hash).Join(e.cacheOutTarName(name)).Abs()
}

func (e *Engine) targetOutputHashFile(target *Target, name string) string {
	return e.cacheDir(target).Join(e.cacheOutHashName(name)).Abs()
}

const versionFile = "version"
const inputHashFile = "hash_input"

func (e *TargetRunEngine) storeCache(ctx context.Context, target *Target, outRoot string) (rerr error) {
	names := target.OutWithSupport.Names()
	names = utils.CopyArray(names)
	names = targetspec.SortOutputsForHashing(names)

	if target.Cache.Enabled {
		e.Status(TargetStatus(target, "Caching..."))
	} else if len(names) > 0 {
		e.Status(TargetStatus(target, "Storing output..."))
	}

	span := e.SpanCacheStore(ctx, target)
	defer func() {
		span.EndError(rerr)
	}()

	inputHash := e.hashInput(target)

	log.Tracef("Store Cache %v %v", target.FQN, inputHash)

	dir := e.cacheDir(target)

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

	log.Tracef("Taring to cache %v", target.FQN)

	for _, name := range names {
		e.Status(TargetOutputStatus(target, name, "Caching..."))

		var paths fs.Paths
		if name == targetspec.SupportFilesOutput {
			paths = target.ActualSupportFiles()
		} else {
			paths = target.ActualOutFiles().Name(name)
		}
		log.Tracef("Creating archive %v %v", target.FQN, name)

		files := make([]tar.TarFile, 0)
		for _, file := range paths {
			if err := ctx.Err(); err != nil {
				return err
			}

			file := file.WithRoot(outRoot)

			files = append(files, tar.TarFile{
				From: file.Abs(),
				To:   file.RelRoot(),
			})
		}

		err = tar.Tar(ctx, files, e.targetOutputTarFile(target, name))
		if err != nil {
			return err
		}

		outputHash := e.hashOutput(target, name)

		err = fs.WriteFileSync(e.targetOutputHashFile(target, name), []byte(outputHash), os.ModePerm)
		if err != nil {
			return err
		}
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

func (e *TargetRunEngine) getCache(target *Target, output string, onlyMeta bool) (bool, error) {
	log.Tracef("locking cache %v|%v", target.FQN, output)
	err := target.cacheLocks[output].Lock()
	if err != nil {
		return false, err
	}

	defer func() {
		log.Tracef("unlocking cache %v|%v", target.FQN, output)
		err := target.cacheLocks[output].Unlock()
		if err != nil {
			log.Errorf("unlocking cache %v %v", target.FQN, err)
		}
	}()

	ok, err := e.getLocalCache(target, output, onlyMeta)
	if err != nil {
		return false, err
	}

	if ok {
		e.Status(TargetOutputStatus(target, output, "Using local cache..."))
		return true, nil
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
			return false, err
		}

		ok, err := e.getVfsCache(loc, cache.Name, target, output, onlyMeta)
		if err != nil {
			log.Errorf("cache %v: %v", cache.Name, err)
			continue
		}

		if ok {
			log.Debugf("%v %v cache hit", target.FQN, cache.Name)
			return e.getLocalCache(target, output, onlyMeta)
		} else {
			log.Debugf("%v %v cache miss", target.FQN, cache.Name)
		}
	}

	return false, nil
}

func (e *Engine) getLocalCache(target *Target, output string, onlyMeta bool) (bool, error) {
	hash := e.hashInput(target)

	dir := e.cacheDir(target)

	cacheHashb, err := os.ReadFile(dir.Join(inputHashFile).Abs())
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}

		return false, err
	}
	cacheHash := string(cacheHashb)

	log.Tracef("Cache %v: %v %v", target.FQN, hash, cacheHash)

	if cacheHash != hash {
		return false, nil
	}

	if output == inputHashName {
		return true, nil
	}

	if !fs.PathExists(e.targetOutputHashFile(target, output)) {
		return false, nil
	}

	if onlyMeta {
		return true, nil
	}

	if !fs.PathExists(e.targetOutputTarFile(target, output)) {
		return false, nil
	}

	err = e.linkLatestCache(target, dir.Abs())
	if err != nil {
		return false, err
	}

	return true, nil
}

func (e *Engine) linkLatestCache(target *Target, from string) error {
	latestDir := e.cacheDirForHash(target, "latest")

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
