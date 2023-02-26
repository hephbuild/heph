package engine

import (
	tar2 "archive/tar"
	"context"
	"errors"
	"fmt"
	log "heph/hlog"
	"heph/targetspec"
	"heph/tgt"
	"heph/utils/fs"
	"heph/utils/hash"
	"heph/utils/tar"
	"io"
	"os"
	"strings"
	"sync"
	"time"
)

func (e *Engine) hashFile(h hash.Hash, file fs.Path) error {
	return e.hashFilePath(h, file.Abs())
}

func (e *Engine) hashDepsTargets(h hash.Hash, targets []tgt.TargetWithOutput) {
	for _, dep := range targets {
		if len(dep.Target.Out.Names()) == 0 {
			continue
		}

		h.String(dep.Target.FQN)
		dh := e.hashOutput(e.Targets.Find(dep.Target), dep.Output)
		h.String(dh)

		if dep.Mode != targetspec.TargetSpecDepModeCopy {
			h.String(string(dep.Mode))
		}
	}
}

func (e *Engine) hashFiles(h hash.Hash, hashMethod string, files fs.Paths) {
	for _, dep := range files {
		h.String(dep.RelRoot())

		switch hashMethod {
		case targetspec.HashFileContent:
			err := e.hashFile(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: hashFile %v %w", dep.Abs(), err))
			}
		case targetspec.HashFileModTime:
			err := e.hashFileModTime(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: hashFileModTime %v %w", dep.Abs(), err))
			}
		default:
			panic(fmt.Sprintf("unhandled hash_input: %v", hashMethod))
		}
	}
}

var copyBufPool = sync.Pool{
	New: func() interface{} {
		return make([]byte, 1000)
	},
}

func (e *Engine) hashFilePath(h hash.Hash, path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		link, err := os.Readlink(path)
		if err != nil {
			return err
		}

		h.String(link)
		return nil
	}

	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open: %w", err)
	}
	defer f.Close()

	return e.hashFileReader(h, info, f)
}

func (e *Engine) hashFilePerm(h hash.Hash, m os.FileMode) {
	// TODO: figure out a way to properly hash file permission, taking into account different umask
	//h.UI32(uint32(m.Perm()))
}

func (e *Engine) hashFileReader(h hash.Hash, info os.FileInfo, f io.Reader) error {
	e.hashFilePerm(h, info.Mode())

	buf := copyBufPool.Get().([]byte)
	defer copyBufPool.Put(buf)

	_, err := io.CopyBuffer(h, f, buf)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *Engine) hashTar(h hash.Hash, tarPath string) error {
	return tar.Walk(context.Background(), tarPath, func(hdr *tar2.Header, r *tar2.Reader) error {
		h.String(hdr.Name)

		switch hdr.Typeflag {
		case tar2.TypeReg:
			err := e.hashFileReader(h, hdr.FileInfo(), io.LimitReader(r, hdr.Size))
			if err != nil {
				return err
			}

			return nil
		case tar2.TypeSymlink:
			e.hashFilePerm(h, hdr.FileInfo().Mode())
			h.String(hdr.Linkname)
		case tar2.TypeDir:
			return nil
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func (e *Engine) hashFileModTime(h hash.Hash, file fs.Path) error {
	return e.hashFileModTimePath(h, file.Abs())
}

func (e *Engine) hashFileModTimePath(h hash.Hash, path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return fmt.Errorf("symlink cannot be hashed")
	}

	e.hashFilePerm(h, info.Mode())
	h.I64(info.ModTime().UnixNano())

	return nil
}

func (e *Engine) ResetCacheHashInput(target *Target) {
	ks := make([]string, 0)

	for _, k := range e.cacheHashInput.Keys() {
		if strings.HasPrefix(k, target.FQN) {
			ks = append(ks, k)
		}
	}

	for _, k := range ks {
		e.cacheHashInput.Delete(k)
	}
}

func (e *Engine) hashInputFiles(h hash.Hash, target *Target) error {
	e.hashFiles(h, targetspec.HashFileModTime, target.Deps.All().Files)

	for _, dep := range target.Deps.All().Targets {
		err := e.hashInputFiles(h, e.Targets.Find(dep.Target))
		if tgt.ErrStopWalk != nil {
			return err
		}
	}

	if target.DifferentHashDeps {
		e.hashFiles(h, targetspec.HashFileModTime, target.HashDeps.Files)

		for _, dep := range target.HashDeps.Targets {
			err := e.hashInputFiles(h, e.Targets.Find(dep.Target))
			if tgt.ErrStopWalk != nil {
				return err
			}
		}
	}

	return nil
}

func (e *Engine) HashInput(target *Target) string {
	return e.hashInput(target)
}

func hashCacheId(target *Target) string {
	idh := hash.NewHash()
	for _, fqn := range target.linkingDeps.FQNs() {
		idh.String(fqn)
	}

	return target.FQN + idh.Sum()
}

func (e *Engine) hashInput(target *Target) string {
	mu := e.cacheHashInputTargetMutex.Get(target.FQN)
	mu.Lock()
	defer mu.Unlock()

	cacheId := hashCacheId(target)

	if h, ok := e.cacheHashInput.GetOk(cacheId); ok {
		return h
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := hash.NewDebuggableHash(target.FQN + "_hash_input")
	h.I64(8) // Force break all caches

	h.String("=")
	for _, dep := range target.Tools.Targets {
		h.String(dep.Name)

		dh := e.hashOutput(e.Targets.Find(dep.Target), dep.Output)
		h.String(dh)
	}

	h.String("=")
	hash.HashArray(h, target.Tools.Hosts, func(tool targetspec.TargetSpecHostTool) string {
		return tool.Name
	})

	h.String("=")
	if target.DifferentHashDeps {
		h.String("=")
		e.hashDepsTargets(h, target.HashDeps.Targets)
		e.hashFiles(h, target.HashFile, target.HashDeps.Files)
	} else {
		h.String("=")
		for _, name := range target.Deps.Names() {
			h.String("=")
			h.String(name)

			deps := target.Deps.Name(name)

			e.hashDepsTargets(h, deps.Targets)
			e.hashFiles(h, target.HashFile, deps.Files)
		}
	}

	h.String("=")
	for _, cmd := range target.Run {
		h.String(cmd)
	}
	h.String(target.Entrypoint)

	if target.IsTextFile() {
		h.String("=")
		h.Write(target.FileContent)
	}

	h.String("=")
	hash.HashArray(h, target.TargetSpec.Out, func(file targetspec.TargetSpecOutFile) string {
		return file.Name + file.Path
	})

	if target.OutInSandbox {
		h.Bool(target.OutInSandbox)
	}

	h.String("=")
	hash.HashMap(h, target.Env, func(k, v string) string {
		return k + v
	})

	h.String("=")
	h.Bool(target.Gen)

	h.String("=")
	h.String(target.SrcEnv.All)
	hash.HashMap(h, target.SrcEnv.Named, func(k, v string) string {
		return k + v
	})
	h.String(target.OutEnv)

	sh := h.Sum()

	e.cacheHashInput.Set(cacheId, sh)

	return sh
}

func (e *Engine) HashOutput(target *Target, output string) string {
	return e.hashOutput(target, output)
}

func (e *Engine) hashOutput(target *Target, output string) string {
	mu := e.cacheHashOutputTargetMutex.Get(target.FQN + "|" + output)
	mu.Lock()
	defer mu.Unlock()

	hashInput := e.hashInput(target)
	cacheId := target.FQN + "|" + output + "_" + hashInput

	if h, ok := e.cacheHashOutput.GetOk(cacheId); ok {
		return h
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashoutput %v|%v took %v", target.FQN, output, time.Since(start))
	}()

	if !target.OutWithSupport.HasName(output) {
		panic(fmt.Sprintf("%v does not output `%v`", target, output))
	}

	file := e.cacheDir(target).Join(target.artifacts.OutHash(output).Name()).Abs()
	b, err := os.ReadFile(file)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Errorf("reading %v: %v", file, err)
	}

	if sh := strings.TrimSpace(string(b)); len(sh) > 0 {
		e.cacheHashOutput.Set(cacheId, sh)

		return sh
	}

	// Sanity check, will bomb if not called in the right order
	_ = target.ActualOutFiles()

	h := hash.NewDebuggableHash(target.FQN + "_hash_out_" + output)

	h.String(output)

	tarPath := e.cacheDir(target).Join(target.artifacts.OutTar(output).Name()).Abs()
	err = e.hashTar(h, tarPath)
	if err != nil {
		panic(fmt.Errorf("hashOutput: %v: hashTar %v %w", target.FQN, tarPath, err))
	}

	if target.HasSupportFiles && output != targetspec.SupportFilesOutput {
		sh := e.hashOutput(target, targetspec.SupportFilesOutput)
		h.String(sh)
	}

	sh := h.Sum()

	e.cacheHashOutput.Set(cacheId, sh)

	return sh
}
