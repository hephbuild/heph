package lcache

import (
	tar2 "archive/tar"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xsync"
	"io"
	"os"
	"regexp"
	"strings"
	"time"
)

func (e *LocalCacheState) hashDepsTargets(h hash.Hash, targets []graph.TargetWithOutput) error {
	for _, dep := range targets {
		if len(dep.Target.Out.Names()) == 0 {
			continue
		}

		h.String(dep.Target.Addr)
		dh, err := e.hashOutput(e.find(dep.Target), dep.Output)
		if err != nil {
			return err
		}
		h.String(dh)

		if dep.Mode != specs.DepModeCopy {
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
		case specs.HashFileContent:
			modtime, err := e.hashFilePath(h, p)
			if err != nil {
				return nil, fmt.Errorf("hashDeps: hashFile %v %w", dep.Abs(), err)
			}
			m[dep.Abs()] = modtime
		case specs.HashFileModTime:
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

func (e *LocalCacheState) find(t specs.Specer) graph.Targeter {
	return e.Targets.FindT(t)
}

func (e *LocalCacheState) HashInput(target graph.Targeter) (string, error) {
	return e.hashInput(target, false)
}

// VerifyHashInput will make sure files haven't been changed since last hashing
func (e *LocalCacheState) VerifyHashInput(target graph.Targeter) (string, error) {
	return e.hashInput(target, true)
}

func (e *LocalCacheState) hashInput(gtarget graph.Targeter, verify bool) (string, error) {
	target := e.Metas.Find(gtarget)

	target.cacheHashInputTargetMutex.Lock()
	defer target.cacheHashInputTargetMutex.Unlock()

	if h := target.inputHash; h != "" {
		if verify {
			for p, t := range target.cacheHashInputPathsModtime {
				info, err := os.Lstat(p)
				if err != nil {
					return "", err
				}

				if info.ModTime() != t {
					return "", fmt.Errorf("%v: %w", p, ErrFileModifiedSinceHashing)
				}
			}
		}

		return h, nil
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashinput %v took %v", target.Addr, time.Since(start))
	}()

	h := hash.NewDebuggableHash(func() string {
		return target.Addr + "_" + target.depsHash + "_hash_input"
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
	hash.HashArray(h, target.Tools.Hosts, func(tool specs.HostTool) string {
		return tool.Name
	})

	allHashDeps := target.Deps
	if !target.HashDeps.Empty() {
		allHashDeps = target.Deps.Copy()
		allHashDeps.Add("_", target.HashDeps)
		allHashDeps.Sort()
	}

	h.String("=") // Legacy reasons...
	h.String("=")
	pathsModtime := make(map[string]time.Time)
	for _, name := range allHashDeps.Names() {
		h.String("=")
		h.String(name)

		deps := allHashDeps.Name(name)

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
	hash.HashArray(h, target.Spec().Out, func(file specs.OutFile) string {
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
	h.Bool(target.IsGen())

	h.String("=")
	h.String(target.SrcEnv.Default)
	hash.HashMap(h, target.SrcEnv.Named, func(k, v string) string {
		return k + v
	})
	h.String(target.OutEnv)

	if target.RestoreCache.Enabled {
		h.String("=")
		h.String(target.RestoreCache.Key)
		hash.HashArray(h, target.RestoreCache.Paths, func(s string) string {
			return s
		})
	}

	target.inputHash = h.Sum()
	target.cacheHashInputPathsModtime = pathsModtime

	return target.inputHash, nil
}

func (e *LocalCacheState) HashOutput(target graph.Targeter, output string) (string, error) {
	return e.hashOutput(target, output)
}

func (e *LocalCacheState) hashArtifact(h hash.Hash, target *graph.Target, artifact artifacts.Artifact) error {
	r, _, err := e.UncompressedReaderFromArtifact(context.TODO(), artifact, target)
	if err != nil {
		return fmt.Errorf("uncompressedreader %v %w", artifact.Name(), err)
	}
	defer r.Close()

	err = e.hashTar(h, r)
	if err != nil {
		return fmt.Errorf("hashTar %v %w", artifact.Name(), err)
	}

	return nil
}

func (e *LocalCacheState) hashOutput(gtarget graph.Targeter, output string) (string, error) {
	target := gtarget.GraphTarget()
	targetm := e.Metas.Find(gtarget)

	mu := targetm.cacheHashOutputTargetMutex.Get(output)
	mu.Lock()
	defer mu.Unlock()

	if h, ok := targetm.cacheHashOutput.GetOk(output); ok {
		return h, nil
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashoutput %v|%v took %v", target.Addr, output, time.Since(start))
	}()

	if !target.OutWithSupport.HasName(output) {
		return "", fmt.Errorf("%v does not output `%v`", target, output)
	}

	dir, err := e.cacheDir(target)
	if err != nil {
		return "", err
	}

	file := dir.Join(target.Artifacts.OutHash(output).FileName()).Abs()
	b, err := os.ReadFile(file)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Errorf("reading %v: %v", file, err)
	}

	if sh := strings.TrimSpace(string(b)); len(sh) > 0 {
		targetm.cacheHashOutput.Set(output, sh)

		return sh, nil
	}

	h := hash.NewDebuggableHash(func() string {
		return target.Addr + "|" + output + "_" + targetm.inputHash + "_hash_out"
	})

	h.String(output)

	err = e.hashArtifact(h, target, target.Artifacts.OutTar(output))
	if err != nil {
		return "", err
	}

	if target.HasSupportFiles && output != specs.SupportFilesOutput {
		sh, err := e.hashOutput(target, specs.SupportFilesOutput)
		if err != nil {
			return "", err
		}
		h.String(sh)
	}

	sh := h.Sum()

	targetm.cacheHashOutput.Set(output, sh)

	return sh, nil
}

func (e *LocalCacheState) cacheDir(target graph.Targeter) (xfs.Path, error) {
	h, err := e.hashInput(target, false)
	if err != nil {
		return xfs.Path{}, err
	}

	return e.cacheDirForHash(target, h), nil
}

func (e *LocalCacheState) cacheDirForHash(target specs.Specer, inputHash string) xfs.Path {
	spec := target.Spec()

	name := SanitizeTargetName(spec.Name)

	// TODO: cache
	folder := "__target_" + name
	if !spec.Cache.Enabled {
		folder = "__target_tmp_" + instance.UID + "_" + name
	}
	return e.Root.Home.Join("cache", spec.Package.Path, folder, inputHash)
}

func lockPath(root *hroot.State, target specs.Specer, resource string) string {
	spec := target.Spec()

	folder := "__target_" + spec.Name
	return root.Tmp.Join(spec.Package.Path, folder, resource+".lock").Abs()
}

var metaEscapeReplacer = regexp.MustCompile("[^a-zA-Z0-9_-]+")

func SanitizeTargetName(s string) string {
	return metaEscapeReplacer.ReplaceAllString(s, "_")
}
