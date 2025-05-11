package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"fmt"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
	"io/fs"
	"path"
	"path/filepath"
	"strings"
)

type Cfg struct {
	Patterns map[string][]string
	Files    map[string]string
}

func relglob(dir, pattern string) ([]string, error) {
	paths, err := filepath.Glob(path.Join(dir, pattern))
	if err == nil && len(paths) == 0 {
		return nil, fmt.Errorf("pattern %s: no matching paths found", pattern)
	}
	ret := make([]string, 0, len(paths))
	for _, p := range paths {
		if err := filepath.WalkDir(p, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			} else if !d.IsDir() {
				ret = append(ret, strings.TrimLeft(strings.TrimPrefix(path, dir), string(filepath.Separator)))
			}
			return nil
		}); err != nil {
			return nil, err
		}
	}
	return ret, err
}

func (p *Plugin) embedCfg(ctx context.Context, basePkg, currentPkg string, goPkg Package, factors Factors, mode string) (*connect.Response[pluginv1.GetResponse], error) {
	var patterns []string
	args := factors.Args()
	switch mode {
	case ModeNormal:
		patterns = goPkg.EmbedPatterns
	case ModeTest:
		patterns = append(goPkg.TestEmbedPatterns, goPkg.EmbedPatterns...)
	case ModeXTest:
		patterns = goPkg.XTestEmbedPatterns
	}

	if mode != "" {
		args["mode"] = mode
	}

	// TODO: move out of linking

	cfg := &Cfg{
		Patterns: map[string][]string{},
		Files:    map[string]string{},
	}

	for _, pattern := range patterns {
		paths, err := relglob(goPkg.Dir, pattern)
		if err != nil {
			return nil, err
		}
		cfg.Patterns[pattern] = paths
		for _, p := range paths {
			cfg.Files[p] = filepath.Join(goPkg.Dir, p)
		}
	}

	b, err := json.Marshal(cfg)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "embedcfg",
				Args:    args,
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("echo %q > $OUT", string(b))),
				"out": structpb.NewStringValue("embedcfg.json"),
			},
		},
	}), nil
}
