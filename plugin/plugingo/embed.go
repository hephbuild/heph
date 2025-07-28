package plugingo

import (
	"context"
	"fmt"
	"io/fs"
	"path"
	"path/filepath"
	"strings"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/goccy/go-json"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
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

func (p *Plugin) embedCfg(ctx context.Context, basePkg, currentPkg string, goPkg Package, factors Factors, mode string) (*pluginv1.GetResponse, error) {
	var patterns []string
	args := factors.Args()
	switch mode {
	case ModeNormal:
		patterns = goPkg.EmbedPatterns
	case ModeTest:
		patterns = append(patterns, goPkg.EmbedPatterns...)
		patterns = append(patterns, goPkg.TestEmbedPatterns...)
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

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
				Name:    htypes.Ptr("embedcfg"),
				Args:    args,
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("echo %q > $OUT", string(b))),
				"out": structpb.NewStringValue("embedcfg.json"),
			},
		}.Build(),
	}.Build(), nil
}
