package engine

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/config"
	"heph/utils"
	"io/fs"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"text/template"
)

var cloneScript = `
git clone {{.URL}} {{.Tmp}} --depth 1
cp -r {{.Tmp}}/{{.Dir}}/. {{.Into}}
`

type RootConfig struct {
	*url.URL
}

func (e *Engine) rootRoot(name string) Path {
	rel, _ := filepath.Rel(e.Root, e.HomeDir)

	return Path{
		Abs:     filepath.Join(e.HomeDir, "root", name),
		RelRoot: filepath.Join(rel, "root", name),
	}
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

func (e *Engine) fetchGitRoot(uri *url.URL, srcRoot Path) (Path, error) {
	guri, err := parseGitURI(uri)
	if err != nil {
		return Path{}, err
	}

	tpl, err := template.New("clone").Parse(cloneScript)
	if err != nil {
		return Path{}, err
	}

	var b bytes.Buffer
	err = tpl.Execute(&b, map[string]interface{}{
		"Tmp":  utils.RandPath(os.TempDir(), "heph_root", ""),
		"URL":  guri.Repo,
		"Into": srcRoot.Abs,
		"Dir":  guri.Path,
	})
	if err != nil {
		return Path{}, err
	}

	cmd := exec.Command("bash", "-eux", "-c", b.String())
	ob, err := cmd.CombinedOutput()
	if err != nil {
		return Path{}, fmt.Errorf("%v: %s", err, ob)
	}

	return srcRoot, nil
}

func (e *Engine) runRootBuildFiles(rootName string, cfg config.Root) error {
	p, err := e.fetchRoot(rootName, cfg)
	if err != nil {
		return err
	}

	err = e.runBuildFiles(p.Abs, func(dir string) *Package {
		rel, err := filepath.Rel(p.Abs, filepath.Join(e.Root, dir))
		if err != nil {
			panic(err)
		}

		return e.getOrCreatePkg(filepath.Join(rootName, rel), func(fullname, name string) *Package {
			return &Package{
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

func (e *Engine) loadFromRoot(pkgName, rootName string, cfg config.Root) (*Package, error) {
	p, err := e.fetchRoot(rootName, cfg)
	if err != nil {
		return nil, err
	}

	rootName, rest := e.splitRootNameFromPkgName(pkgName)

	pkgPath := p.Join(rest)

	pkg := e.getOrCreatePkg(pkgName, func(fullname, name string) *Package {
		return &Package{
			Name:     name,
			FullName: fullname,
			Root:     pkgPath,
		}
	})

	return pkg, nil
}

func (e *Engine) fetchRoot(name string, cfg config.Root) (Path, error) {
	log.Tracef("fetchRoot %v", name)

	root := e.rootRoot(name)
	srcRoot := root.Join("src")
	metaPath := root.Join("meta").Abs

	b, err := os.ReadFile(metaPath)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return Path{}, err
	}

	var fileCfg config.Root
	if len(b) > 0 {
		err = json.Unmarshal(b, &fileCfg)
		if err != nil {
			log.Errorf("reading %v root meta: %v", name, err)
		}
	}

	if fileCfg.URI == cfg.URI {
		return srcRoot, nil
	}

	log.Infof("Fetch root %v from %v", name, cfg.URI)

	err = os.RemoveAll(root.Abs)
	if err != nil {
		return Path{}, err
	}

	err = os.MkdirAll(srcRoot.Abs, os.ModePerm)
	if err != nil {
		return Path{}, err
	}

	u, err := url.Parse(cfg.URI)
	if err != nil {
		return Path{}, err
	}

	var backendRoot Path
	switch u.Scheme {
	case "git":
		backendRoot, err = e.fetchGitRoot(u, srcRoot)
		if err != nil {
			return Path{}, err
		}
	default:
		return Path{}, fmt.Errorf("unsupported scheme %v", u.Scheme)
	}

	metaFile, err := os.Create(metaPath)
	if err != nil {
		return Path{}, err
	}
	defer metaFile.Close()

	enc := json.NewEncoder(metaFile)
	enc.SetEscapeHTML(false)

	err = enc.Encode(cfg)
	if err != nil {
		return Path{}, err
	}

	return backendRoot, nil
}

func (e *Engine) splitRootNameFromPkgName(pkgName string) (root string, rest string) {
	parts := strings.SplitN(pkgName, "/", 2)
	if len(parts) == 0 {
		return "", ""
	}

	return parts[0], strings.Join(parts[1:], "/")
}

func (e *Engine) loadFromRoots(pkgName string) (*Package, error) {
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