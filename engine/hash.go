package engine

import (
	tar2 "archive/tar"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xsync"
	"io"
	"os"
	"strings"
	"time"
)

func (e *LocalCacheState) hashDepsTargets(h hash.Hash, targets []tgt.TargetWithOutput) error {
	for _, dep := range targets {
		if len(dep.Target.Out.Names()) == 0 {
			continue
		}

		h.String(dep.Target.FQN)
		dh, err := e.hashOutput(e.find(dep.Target), dep.Output)
		if err != nil {
			return err
		}
		h.String(dh)

		if dep.Mode != targetspec.TargetSpecDepModeCopy {
			h.String(string(dep.Mode))
		}
	}

	return nil
}

func (e *LocalCacheState) hashFiles(h hash.Hash, hashMethod string, files xfs.Paths) (map[string]time.Time, error) {
	m := make(map[string]time.Time, len(files))

	for _, dep := range files {
		h.String(dep.RelRoot())
		p := dep.Abs()

		switch hashMethod {
		case targetspec.HashFileContent:
			modtime, err := e.hashFilePath(h, p)
			if err != nil {
				return nil, fmt.Errorf("hashDeps: hashFile %v %w", dep.Abs(), err)
			}
			m[dep.Abs()] = modtime
		case targetspec.HashFileModTime:
			modtime, err := e.hashFileModTimePath(h, p)
			if err != nil {
				return nil, fmt.Errorf("hashDeps: hashFileModTime %v %w", dep.Abs(), err)
			}
			m[dep.Abs()] = modtime
		default:
			return nil, fmt.Errorf("unhandled hash method: %v", hashMethod)
		}
	}

	return m, nil
}

var ErrFileModifiedWhileHashing = errors.New("modified while hashing")

func (e *LocalCacheState) hashFilePath(h hash.Hash, path string) (time.Time, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return time.Time{}, fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		link, err := os.Readlink(path)
		if err != nil {
			return time.Time{}, err
		}

		h.String(link)
		return info.ModTime(), nil
	}

	before := info.ModTime()

	f, err := os.Open(path)
	if err != nil {
		return time.Time{}, fmt.Errorf("open: %w", err)
	}

	err = e.hashFileReader(h, info, f)
	_ = f.Close()
	if err != nil {
		return time.Time{}, err
	}

	info, err = os.Lstat(path)
	if err != nil {
		return time.Time{}, fmt.Errorf("stat: %w", err)
	}

	after := info.ModTime()

	if before != after {
		return time.Time{}, fmt.Errorf("%v: %w", path, ErrFileModifiedWhileHashing)
	}

	return after, nil
}

var ErrFileModifiedSinceHashing = errors.New("modified since hashing")

func (e *LocalCacheState) hashFilePerm(h hash.Hash, m os.FileMode) {
	// TODO: figure out a way to properly hash file permission, taking into account different umask
	// See: https://medium.com/@tahteche/how-git-treats-changes-in-file-permissions-f71874ca239d
	//h.UI32(uint32(m.Perm()))
}

var copyBufPool = xsync.Pool[[]byte]{
	New: func() []byte {
		return make([]byte, 32*1024)
	},
}

func (e *LocalCacheState) hashFileReader(h hash.Hash, info os.FileInfo, f io.Reader) error {
	e.hashFilePerm(h, info.Mode())

	buf := copyBufPool.Get()
	defer copyBufPool.Put(buf)

	_, err := io.CopyBuffer(h, f, buf)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *LocalCacheState) hashTar(h hash.Hash, r io.Reader) error {
	return tar.Walk(r, func(hdr *tar2.Header, r *tar2.Reader) error {
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

func (e *LocalCacheState) hashFileModTimePath(h hash.Hash, path string) (time.Time, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return time.Time{}, fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return time.Time{}, fmt.Errorf("symlink cannot be hashed")
	}

	modtime := info.ModTime()

	e.hashFilePerm(h, info.Mode())
	h.I64(modtime.UnixNano())

	return modtime, nil
}

func (e *LocalCacheState) find(t targetspec.Specer) graph.Targeter {
	return e.Targets.Find(t.Spec().FQN)
}

func (e *LocalCacheState) HashInput(target graph.Targeter) string {
	return e.mustHashInput(target)
}

func hashCacheId(gtarget graph.Targeter) targetCacheKey {
	target := gtarget.GraphTarget()

	idh := hash.NewHash()
	for _, fqn := range target.AllTargetDeps.FQNs() {
		idh.String(fqn)
	}

	return targetCacheKey{
		fqn:  target.FQN,
		hash: idh.Sum(),
	}
}

func (e *LocalCacheState) mustHashInput(target graph.Targeter) string {
	h, err := e.hashInput(target, false)
	if err != nil {
		panic(err)
	}
	return h
}

func (e *LocalCacheState) hashInput(gtarget graph.Targeter, safe bool) (string, error) {
	target := gtarget.GraphTarget()

	mu := e.cacheHashInputTargetMutex.Get(target.FQN)
	mu.Lock()
	defer mu.Unlock()

	cacheId := hashCacheId(target)

	if h, ok := e.cacheHashInput.GetOk(cacheId); ok {
		if safe {
			if m, ok := e.cacheHashInputPathsModtime.GetOk(cacheId); ok {
				for p, t := range m {
					info, err := os.Lstat(p)
					if err != nil {
						return "", err
					}

					if info.ModTime() != t {
						return "", fmt.Errorf("%v: %w", p, ErrFileModifiedSinceHashing)
					}
				}
			}
		}

		return h, nil
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := hash.NewDebuggableHash(func() string {
		return cacheId.String() + "_hash_input"
	})
	h.I64(8) // Force break all caches

	h.String("=")
	for _, dep := range target.Tools.Targets {
		h.String(dep.Name)

		dh, err := e.hashOutput(e.find(dep.Target), dep.Output)
		if err != nil {
			return "", err
		}
		h.String(dh)
	}

	h.String("=")
	hash.HashArray(h, target.Tools.Hosts, func(tool targetspec.TargetSpecHostTool) string {
		return tool.Name
	})

	h.String("=")
	var pathsModtime map[string]time.Time
	if target.DifferentHashDeps {
		h.String("=")
		err := e.hashDepsTargets(h, target.HashDeps.Targets)
		if err != nil {
			return "", err
		}
		pathsModtime, err = e.hashFiles(h, target.HashFile, target.HashDeps.Files)
		if err != nil {
			return "", err
		}
	} else {
		h.String("=")
		pathsModtime = make(map[string]time.Time)
		for _, name := range target.Deps.Names() {
			h.String("=")
			h.String(name)

			deps := target.Deps.Name(name)

			err := e.hashDepsTargets(h, deps.Targets)
			if err != nil {
				return "", err
			}

			m, err := e.hashFiles(h, target.HashFile, deps.Files)
			if err != nil {
				return "", err
			}

			for p, t := range m {
				if pt, ok := pathsModtime[p]; ok {
					if t != pt {
						return "", fmt.Errorf("%v: %w", p, ErrFileModifiedWhileHashing)
					}
				} else {
					pathsModtime[p] = t
				}
			}
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
	h.String(target.SrcEnv.Default)
	hash.HashMap(h, target.SrcEnv.Named, func(k, v string) string {
		return k + v
	})
	h.String(target.OutEnv)

	sh := h.Sum()

	e.cacheHashInput.Set(cacheId, sh)
	e.cacheHashInputPathsModtime.Set(cacheId, pathsModtime)

	return sh, nil
}

func (e *LocalCacheState) HashOutput(target graph.Targeter, output string) string {
	return e.mustHashOutput(target, output)
}

func (e *LocalCacheState) mustHashOutput(target graph.Targeter, output string) string {
	h, err := e.hashOutput(target, output)
	if err != nil {
		panic(err)
	}

	return h
}

func (e *LocalCacheState) hashOutput(gtarget graph.Targeter, output string) (string, error) {
	target := gtarget.GraphTarget()

	mu := e.cacheHashOutputTargetMutex.Get(target.FQN + "|" + output)
	mu.Lock()
	defer mu.Unlock()

	hashInput, err := e.hashInput(target, false)
	if err != nil {
		return "", err
	}

	cacheId := targetOutCacheKey{
		fqn:    target.FQN,
		output: output,
		hash:   hashInput,
	}

	if h, ok := e.cacheHashOutput.GetOk(cacheId); ok {
		return h, nil
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashoutput %v|%v took %v", target.FQN, output, time.Since(start))
	}()

	if !target.OutWithSupport.HasName(output) {
		return "", fmt.Errorf("%v does not output `%v`", target, output)
	}

	file := e.cacheDir(target).Join(target.Artifacts.OutHash(output).FileName()).Abs()
	b, err := os.ReadFile(file)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Errorf("reading %v: %v", file, err)
	}

	if sh := strings.TrimSpace(string(b)); len(sh) > 0 {
		e.cacheHashOutput.Set(cacheId, sh)

		return sh, nil
	}

	h := hash.NewDebuggableHash(func() string {
		return cacheId.String() + "_hash_out"
	})

	h.String(output)

	r, err := artifacts.UncompressedReaderFromArtifact(target.Artifacts.OutTar(output), e.cacheDir(target).Abs())
	if err != nil {
		return "", fmt.Errorf("hashOutput: %v: uncompressedreader %v %w", target.FQN, output, err)
	}
	defer r.Close()

	err = e.hashTar(h, r)
	if err != nil {
		return "", fmt.Errorf("hashOutput: %v: hashTar %v %w", target.FQN, output, err)
	}

	if target.HasSupportFiles && output != targetspec.SupportFilesOutput {
		sh, err := e.hashOutput(target, targetspec.SupportFilesOutput)
		if err != nil {
			return "", err
		}
		h.String(sh)
	}

	sh := h.Sum()

	e.cacheHashOutput.Set(cacheId, sh)

	return sh, nil
}

func (e *LocalCacheState) cacheDir(target graph.Targeter) xfs.Path {
	return e.cacheDirForHash(target, e.mustHashInput(target))
}

func (e *LocalCacheState) cacheDirForHash(target targetspec.Specer, inputHash string) xfs.Path {
	spec := target.Spec()

	// TODO: cache
	folder := "__target_" + spec.Name
	if !spec.Cache.Enabled {
		folder = "__target_tmp_" + instance.UID + "_" + spec.Name
	}
	return e.Root.Home.Join("cache", spec.Package.Path, folder, inputHash)
}
