package hephprovider

import (
	"fmt"
	log "heph/hlog"
	"heph/utils"
	fs2 "heph/utils/fs"
	"io/fs"
	"os"
	"path/filepath"
)

const (
	EnvDistRoot = "HEPH_DIST_ROOT"
	EnvSrcRoot  = "HEPH_SRC_ROOT"
)

func GetHephPath(outDir, goos, goarch string, shouldBuildAll bool) (string, string, error) {
	if root := os.Getenv(EnvDistRoot); root != "" {
		if outDir != root {
			// In the case of e2e test, a sandbox is created inside the sandbox
			err := fs2.CpHardlink(root, outDir)
			if err != nil {
				return "", "", err
			}

			root = outDir
		}

		p := filepath.Join(root, hephBinName(goos, goarch))

		if !fs2.PathExists(p) {
			return "", "", fmt.Errorf("hephbuild: dist: %v: %w", p, fs.ErrNotExist)
		}

		return p, root, nil
	}

	if utils.IsDevVersion() {
		if srcDir := os.Getenv(EnvSrcRoot); srcDir != "" {
			if !shouldBuildAll {
				log.Infof("Building heph %v/%v...", goos, goarch)
				p := filepath.Join(outDir, hephBinName(goos, goarch))

				err := build(srcDir, goos, goarch, p)
				if err != nil {
					return "", "", err
				}

				return p, "", nil
			}

			m, err := buildAll(srcDir, outDir)
			if err != nil {
				return "", "", err
			}

			p, ok := m.GetOk(key(goos, goarch))
			if !ok {
				return "", "", fmt.Errorf("hephbuild: invalid os/arch: %v/%v, expected one of %v", goos, goarch, m.Keys())
			}

			if !fs2.PathExists(p) {
				return "", "", fmt.Errorf("hephbuild: %v: %w", p, fs.ErrNotExist)
			}

			return p, outDir, nil
		}

		return "", "", fmt.Errorf("hephbuild: must provide %v or %v", EnvSrcRoot, EnvDistRoot)
	}

	p, err := Download(outDir, "", utils.Version, goos, goarch)
	if err != nil {
		return "", "", err
	}

	return p, outDir, nil
}
