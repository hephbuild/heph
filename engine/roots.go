package engine

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/utils/flock"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"io/fs"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"text/template"
)

var cloneScript = `
git clone {{.URL}} {{.Tmp}}
cd {{.Tmp}} && git reset --hard {{.Ref}}
cp -r {{.Tmp}}/{{.Dir}}/. {{.Into}}
`

type RootConfig struct {
	*url.URL
}

func (e *Engine) rootRoot(name string) fs2.Path {
	return e.HomeDir.Join("root", name)
}

type GitURI struct {
	Repo string
	Ref  string
	Path string
}

func parseGitURI(uri *url.URL) (GitURI, error) {
	parts1 := strings.SplitN(uri.Path, "@", 2)
	parts2 := strings.SplitN(parts1[1], ":", 2)

	var path string
	if len(parts2) > 1 {
		path = parts2[1]
	}

	return GitURI{
		Repo: fmt.Sprintf("https://%v%v", uri.Host, parts1[0]),
		Ref:  parts2[0],
		Path: path,
	}, nil
}

func (e *Engine) fetchGitRoot(ctx context.Context, uri *url.URL, srcRoot fs2.Path) (fs2.Path, error) {
	err := os.MkdirAll(srcRoot.Abs(), os.ModePerm)
	if err != nil {
		return fs2.Path{}, err
	}

	guri, err := parseGitURI(uri)
	if err != nil {
		return fs2.Path{}, err
	}

	tpl, err := template.New("clone").Parse(cloneScript)
	if err != nil {
		return fs2.Path{}, err
	}

	tmp := fs2.RandPath(os.TempDir(), "heph_root", "")
	defer os.RemoveAll(tmp)

	var b bytes.Buffer
	err = tpl.Execute(&b, map[string]interface{}{
		"Tmp":  tmp,
		"URL":  guri.Repo,
		"Into": srcRoot.Abs(),
		"Dir":  guri.Path,
		"Ref":  guri.Ref,
	})
	if err != nil {
		return fs2.Path{}, err
	}

	cmd := exec.CommandContext(ctx, "bash", "-eux", "-c", b.String())
	ob, err := cmd.CombinedOutput()
	if err != nil {
		return fs2.Path{}, fmt.Errorf("%v: %s", err, ob)
	}

	return srcRoot, nil
}

func (e *Engine) fetchFsRoot(uri *url.URL) fs2.Path {
	return e.Root.Join(strings.TrimPrefix(uri.Path, "/"))
}

func (e *Engine) runRootBuildFiles(ctx context.Context, rootName string, cfg config.Root) error {
	p, err := e.fetchRoot(ctx, rootName, cfg)
	if err != nil {
		return err
	}

	err = e.runBuildFiles(p.Abs(), func(dir string) *packages.Package {
		rel, err := filepath.Rel(p.Abs(), e.Root.Join(dir).Abs())
		if err != nil {
			panic(err)
		}

		return e.getOrCreatePkg(filepath.Join(rootName, rel), func(fullname, name string) *packages.Package {
			return &packages.Package{
				Name:     name,
				FullName: fullname,
				Root:     p.Join(rel),
			}
		})
	})
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) loadFromRoot(pkgName, rootName string, cfg config.Root) (*packages.Package, error) {
	p, err := e.fetchRoot(context.TODO(), rootName, cfg)
	if err != nil {
		return nil, err
	}

	rootName, rest := e.splitRootNameFromPkgName(pkgName)

	pkgPath := p.Join(rest)

	pkg := e.getOrCreatePkg(pkgName, func(fullname, name string) *packages.Package {
		return &packages.Package{
			Name:     name,
			FullName: fullname,
			Root:     pkgPath,
		}
	})

	return pkg, nil
}

func (e *Engine) fetchRoot(ctx context.Context, name string, cfg config.Root) (fs2.Path, error) {
	if p, ok := e.fetchRootCache[name]; ok {
		return p, nil
	}

	lock := flock.NewFlock("root "+name, filepath.Join(e.HomeDir.Abs(), "root_"+name+".lock"))
	err := lock.Lock(ctx)
	if err != nil {
		return fs2.Path{}, fmt.Errorf("Failed to lock %v", err)
	}
	defer lock.Unlock()

	log.Tracef("fetchRoot %v", name)

	u, err := url.Parse(cfg.URI)
	if err != nil {
		return fs2.Path{}, err
	}

	if u.Scheme == "file" {
		root := e.fetchFsRoot(u)

		e.fetchRootCache[name] = root
		return root, nil
	}

	root := e.rootRoot(name)
	srcRoot := root.Join("src")
	metaPath := root.Join("meta").Abs()

	b, err := os.ReadFile(metaPath)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fs2.Path{}, err
	}

	var fileCfg config.Root
	if len(b) > 0 {
		err = json.Unmarshal(b, &fileCfg)
		if err != nil {
			log.Errorf("reading %v root meta: %v", name, err)
		}
	}

	if fileCfg.URI == cfg.URI {
		e.fetchRootCache[name] = srcRoot
		return srcRoot, nil
	}

	log.Infof("Fetch root %v from %v", name, cfg.URI)

	err = os.RemoveAll(root.Abs())
	if err != nil {
		return fs2.Path{}, err
	}

	err = os.MkdirAll(root.Abs(), os.ModePerm)
	if err != nil {
		return fs2.Path{}, err
	}

	var backendRoot fs2.Path
	switch u.Scheme {
	case "git":
		backendRoot, err = e.fetchGitRoot(ctx, u, srcRoot)
		if err != nil {
			return fs2.Path{}, err
		}
	case "file":
		backendRoot = e.fetchFsRoot(u)
	default:
		return fs2.Path{}, fmt.Errorf("unsupported scheme %v", u.Scheme)
	}

	metaFile, err := os.Create(metaPath)
	if err != nil {
		return fs2.Path{}, err
	}
	defer metaFile.Close()

	enc := json.NewEncoder(metaFile)
	enc.SetEscapeHTML(false)

	err = enc.Encode(cfg)
	if err != nil {
		return fs2.Path{}, err
	}

	e.fetchRootCache[name] = backendRoot

	return backendRoot, nil
}

func (e *Engine) splitRootNameFromPkgName(pkgName string) (string, string) {
	i := strings.Index(pkgName, "/")
	if i <= 0 {
		return "", ""
	}

	root := pkgName[:i]
	rest := pkgName[i+1:]

	return root, rest
}

func (e *Engine) loadFromRoots(pkgName string) (*packages.Package, error) {
	rootName, _ := e.splitRootNameFromPkgName(pkgName)
	if len(rootName) == 0 {
		return nil, nil
	}

	for root, cfg := range e.Config.BuildFiles.Roots {
		if rootName == root {
			log.Tracef("loading %v from %v", pkgName, root)
			pkg, err := e.loadFromRoot(pkgName, root, cfg)
			if err != nil {
				return nil, err
			}

			return pkg, nil
		}
	}

	return nil, nil
}
