package packages

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/log/log"
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

func (e *Registry) loadFromRoot(pkgName, rest, rootName string, cfg config.Root) (*Package, error) {
	p, err := e.FetchRoot(context.TODO(), rootName, cfg)
	if err != nil {
		return nil, err
	}

	pkg := e.GetOrCreate(Package{
		Path: pkgName,
		Root: p.Join(rest),
	})

	return pkg, nil
}

func (e *Registry) FetchRoot(ctx context.Context, name string, cfg config.Root) (fs2.Path, error) {
	if p, ok := e.fetchRootCache[name]; ok {
		return p, nil
	}

	lock := flock.NewFlock("root "+name, filepath.Join(e.RootsCachePath, "root_"+name+".lock"))
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

	root := e.HomeRoot.Join("root", name)
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
	err = enc.Encode(cfg)
	if err != nil {
		return fs2.Path{}, err
	}

	e.fetchRootCache[name] = backendRoot

	return backendRoot, nil
}

func (e *Registry) splitRootNameFromPkgName(pkgName string) (string, string) {
	i := strings.Index(pkgName, "/")
	if i < 0 {
		return "", ""
	}

	root := pkgName[:i]
	rest := pkgName[i+1:]

	return root, rest
}

func (e *Registry) LoadFromRoots(pkgName string) (*Package, error) {
	rootName, rest := e.splitRootNameFromPkgName(pkgName)
	if len(rootName) == 0 {
		return nil, nil
	}

	for root, cfg := range e.Roots {
		if rootName == root {
			log.Tracef("loading %v from %v", pkgName, root)
			pkg, err := e.loadFromRoot(pkgName, rest, root, cfg)
			if err != nil {
				return nil, err
			}

			return pkg, nil
		}
	}

	return nil, nil
}

var cloneScript = `
git clone {{.URL}} {{.Tmp}}
cd {{.Tmp}} && git reset --hard {{.Ref}}
cp -r {{.Tmp}}/{{.Dir}}/. {{.Into}}
`

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

func (e *Registry) fetchGitRoot(ctx context.Context, uri *url.URL, srcRoot fs2.Path) (fs2.Path, error) {
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

func (e *Registry) fetchFsRoot(uri *url.URL) fs2.Path {
	return e.RepoRoot.Join(strings.TrimPrefix(uri.Path, "/"))
}
