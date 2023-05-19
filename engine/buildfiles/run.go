package buildfiles

import (
	"bufio"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/vmihailenco/msgpack/v5"
	"go.starlark.net/starlark"
	"io"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"strings"
	"sync"
)

type RunOptions struct {
	ThreadModifier  func(thread *starlark.Thread, pkg *packages.Package)
	UniverseFactory func() starlark.StringDict
	CacheDirPath    string
	BuildHash       func(hash.Hash)
	Packages        *packages.Registry
	RootPkg         *packages.Package
}

func (o RunOptions) Copy() RunOptions {
	return o
}

type runContext struct {
	RunOptions
	cacheGlobals *maps.Map[string, starlark.StringDict]
	cacheLocks   *maps.Map[string, *sync.Mutex]

	cacheLoads *maps.Map[string, starlark.StringDict]
}

type breadcrumb struct {
	files []string
	pkgs  []string
}

func (b *breadcrumb) addFile(p string) (*breadcrumb, error) {
	if b == nil {
		return &breadcrumb{
			files: []string{p},
		}, nil
	}

	if ads.Contains(b.files, p) {
		return nil, fmt.Errorf("cyclic dependency: %v", append(b.files, p))
	}

	return &breadcrumb{
		files: append(b.files, p),
		pkgs:  b.pkgs,
	}, nil
}

func (b *breadcrumb) addPkg(p *packages.Package) (*breadcrumb, error) {
	addr := p.Addr()

	if b == nil {
		return &breadcrumb{
			pkgs: []string{addr},
		}, nil
	}

	if ads.Contains(b.pkgs, addr) {
		return nil, fmt.Errorf("cyclic dependency: %v", append(b.pkgs, addr))
	}

	return &breadcrumb{
		files: b.files,
		pkgs:  append(b.pkgs, addr),
	}, nil
}

func NewStarlarkThread() *starlark.Thread {
	return &starlark.Thread{
		Print: func(thread *starlark.Thread, msg string) {
			log.Info(msg)
		},
	}
}

func (e *runContext) runBuildFilesForPackage(pkg *packages.Package, bc *breadcrumb) error {
	if pkg.Globals != nil {
		return nil
	}

	nbc, err := bc.addPkg(pkg)
	if err != nil {
		return err
	}

	pkg.Globals = starlark.StringDict{}
	for _, file := range pkg.SourceFiles {
		globals, err := e.RunBuildFile(pkg, file.Path, nbc)
		if err != nil {
			return err
		}

		for k, v := range globals {
			pkg.Globals[k] = v
		}
	}

	return nil
}

func (e *runContext) RunBuildFile(pkg *packages.Package, path string, bc *breadcrumb) (starlark.StringDict, error) {
	if globals, ok := e.cacheGlobals.GetOk(path); ok {
		return globals, nil
	}

	nbc, err := bc.addFile(path)
	if err != nil {
		return nil, err
	}

	lock := e.cacheLocks.Get(path)
	lock.Lock()
	defer lock.Unlock()

	log.Tracef("BUILD: running %v", path)

	thread := NewStarlarkThread()
	thread.Load = e.load
	thread.SetLocal("__bc", nbc)
	e.ThreadModifier(thread, pkg)

	universe := e.UniverseFactory()

	prog, err := e.buildProgram(path, universe)
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v:\n%v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}

	res, err := prog.Init(thread, universe)
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v:\n%v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}
	res.Freeze()

	e.cacheGlobals.Set(path, res)

	return res, nil
}

func (e *runContext) loadFromRootsOrCreatePackage(pkgName string) (*packages.Package, error) {
	pkg, err := e.Packages.LoadFromRoots(pkgName)
	if err != nil {
		return nil, err
	}

	if pkg == nil {
		pkg = e.Packages.GetOrCreate(e.RootPkg.Child(pkgName))
	}

	return pkg, nil
}

func (e *runContext) load(thread *starlark.Thread, module string) (starlark.StringDict, error) {
	if globals, ok := e.cacheLoads.GetOk(module); ok {
		return globals, nil
	}

	p, err := targetspec.TargetParse("", module)
	if err != nil {
		return nil, err
	}

	bc := thread.Local("__bc").(*breadcrumb)

	dirPkg, file := path.Split(p.Package)
	dirPkg = strings.TrimSuffix(dirPkg, "/")
	if file != "" {
		pkg, err := e.loadFromRootsOrCreatePackage(dirPkg)
		if err != nil {
			return nil, err
		}

		p := pkg.Root.Join(file).Abs()

		if fs2.PathExists(p) {
			info, _ := os.Lstat(p)
			if info.Mode().IsRegular() {
				globals, err := e.RunBuildFile(pkg, p, bc)
				if err != nil {
					return nil, err
				}

				e.cacheLoads.Set(module, globals)

				return globals, nil
			}
		}
	}

	pkg, err := e.loadFromRootsOrCreatePackage(p.Package)
	if err != nil {
		return nil, err
	}

	err = e.runBuildFilesForPackage(pkg, bc)
	if err != nil {
		return nil, fmt.Errorf("load: %w", err)
	}

	e.cacheLoads.Set(module, pkg.Globals)

	return pkg.Globals, nil
}

func safelyCompileProgram(in io.Reader) (_ *starlark.Program, err error) {
	// Sometimes starlark.CompiledProgram bombs when concurrent read/write on the file is happening...

	defer func() {
		if rerr := recover(); rerr != nil {
			err = fmt.Errorf("buildProgram paniced: %v", rerr)
		}
	}()

	return starlark.CompiledProgram(in)
}

type encodableProgram struct {
	prog *starlark.Program
}

func (m encodableProgram) EncodeMsgpack(enc *msgpack.Encoder) error {
	return m.prog.Write(enc.Writer())
}

func (m *encodableProgram) DecodeMsgpack(dec *msgpack.Decoder) error {
	p, err := safelyCompileProgram(dec.Buffered())
	if err != nil {
		return err
	}

	*m = encodableProgram{prog: p}
	return nil
}

type progCache struct {
	Hash string
	Data encodableProgram
}

func (e *runContext) buildProgram(path string, universe starlark.StringDict) (*starlark.Program, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}

	h := hash.NewHash()
	h.I64(info.ModTime().Unix())
	h.UI32(uint32(info.Mode().Perm()))
	h.String(utils.Version)
	e.BuildHash(h)

	sum := h.Sum()

	cachePath := filepath.Join(e.CacheDirPath, hash.HashString(path))

	log.Debugf("BUILD: %v: cache path: %v", path, cachePath)

	cfr, err := os.Open(cachePath)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return nil, err
	}

	if cfr != nil {
		cfb := bufio.NewReader(cfr)

		var c progCache
		err := msgpack.NewDecoder(cfb).Decode(&c)
		_ = cfr.Close()
		if err == nil && c.Hash == sum {
			log.Debugf("BUILD: %v: compiled: %v", path, err)

			return c.Data.prog, nil
		}
		log.Debugf("BUILD: %v: compiled mismatch: expected %v, got %v, err: %v", path, sum, c.Hash, err)
	} else {
		log.Debugf("BUILD: %v: no cache", path)
	}

	_, prog, err := starlark.SourceProgram(path, nil, universe.Has)
	if err != nil {
		return nil, err
	}

	err = fs2.CreateParentDir(cachePath)
	if err != nil {
		return nil, err
	}

	cfw, err := fs2.AtomicCreate(cachePath)
	if err != nil {
		return nil, err
	}
	defer cfw.Close()

	err = msgpack.NewEncoder(cfw).Encode(progCache{
		Hash: sum,
		Data: encodableProgram{prog: prog},
	})
	if err != nil {
		return nil, err
	}

	return prog, nil
}
