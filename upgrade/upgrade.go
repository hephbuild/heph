package upgrade

import (
	"context"
	"fmt"
	"github.com/coreos/go-semver/semver"
	"github.com/mitchellh/go-homedir"
	"heph/config"
	"heph/hephprovider"
	log "heph/hlog"
	"heph/utils"
	"heph/utils/flock"
	"io"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
)

func CheckAndUpdate(ctx context.Context, cfg config.Config) error {
	homeDir, err := homeDir(cfg)
	if err != nil {
		return err
	}

	if !shouldUpdate(cfg) {
		return nil
	}

	lock := flock.NewFlock("heph update", filepath.Join(homeDir, "update.lock"))
	err = lock.Lock(ctx)
	if err != nil {
		return fmt.Errorf("Failed to lock %v", err)
	}
	defer lock.Unlock()

	if !shouldUpdate(cfg) {
		return nil
	}

	log.Infof("Updating %v to %v", utils.Version, cfg.Version.String)

	newHeph, err := downloadAndLink(cfg)
	if err != nil {
		return err
	}

	// Now run the new one.
	args := append([]string{newHeph}, os.Args[1:]...)
	log.Infof("Executing %s", strings.Join(args, " "))
	if err := syscall.Exec(newHeph, args, os.Environ()); err != nil {
		return fmt.Errorf("Failed to exec new Heph version %s: %s", newHeph, err)
	}
	// Shouldn't ever get here. We should have either exec'd or died above.
	panic("heph update failed in an an unexpected and exciting way")
}

func shouldUpdate(cfg config.Config) bool {
	if utils.IsDevVersion() {
		// We are in dev, never update
		return false
	}

	if cfg.Version.String == "latest" || cfg.Version.String == "" {
		// We are already running heph, all good
		return false
	}

	version, _ := semver.NewVersion(utils.Version)

	if version == nil || cfg.Version.Semver == nil {
		// Is not semver, rely on exact version match
		return cfg.Version.String != utils.Version
	}

	if cfg.Version.GTE {
		return version.LessThan(*cfg.Version.Semver)
	} else {
		return !cfg.Version.Semver.Equal(*version)
	}
}

func findLatestVersion() (string, error) {
	res, err := http.Get(fmt.Sprintf("%v/latest_version", hephprovider.BaseUrl))
	if err != nil {
		return "", err
	}
	defer res.Body.Close()

	b, err := io.ReadAll(res.Body)
	if err != nil {
		return "", err
	}

	if res.StatusCode != http.StatusOK {
		return "", fmt.Errorf("http status: %v", res.StatusCode)
	}

	return strings.TrimSpace(string(b)), nil
}

func homeDir(cfg config.Config) (string, error) {
	if cfg.Location != "" {
		return cfg.Location, nil
	}

	home, err := homedir.Dir()
	if err != nil {
		return "", err
	}

	return filepath.Join(home, ".heph"), nil
}

func downloadAndLink(cfg config.Config) (string, error) {
	homeDir, err := homeDir(cfg)
	if err != nil {
		return "", err
	}

	dir := filepath.Join(homeDir, cfg.Version.String)

	dstPath, err := hephprovider.Download(dir, "heph", cfg.Version.String, runtime.GOOS, runtime.GOARCH)
	if err != nil {
		return "", err
	}

	err = linkNew(dir, cfg)
	if err != nil {
		return "", err
	}

	return dstPath, nil
}

func linkNew(newDir string, cfg config.Config) error {
	files, err := os.ReadDir(newDir)
	if err != nil {
		return err
	}

	for _, file := range files {
		err := linkNewFile(newDir, file.Name(), cfg)
		if err != nil {
			return err
		}
	}

	return nil
}

func linkNewFile(newDir, file string, cfg config.Config) error {
	homeDir, err := homeDir(cfg)
	if err != nil {
		return err
	}

	globalFile := path.Join(homeDir, file)
	downloadedFile := path.Join(newDir, file)

	if err := os.RemoveAll(globalFile); err != nil {
		return fmt.Errorf("Failed to remove existing file %s: %s", globalFile, err)
	}
	if err := os.Symlink(downloadedFile, globalFile); err != nil {
		return fmt.Errorf("Error linking %s -> %s: %s", downloadedFile, globalFile, err)
	}

	log.Infof("Linked %s -> %s", globalFile, downloadedFile)

	return nil
}
