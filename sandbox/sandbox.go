package sandbox

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

type MakeConfig struct {
	Root   string
	Bin    map[string]string
	Src    []utils.TarFile
	SrcTar []string
}

func (c MakeConfig) Dir() string {
	return filepath.Join(c.Root, "_dir")
}

func (c MakeConfig) BinDir() string {
	return filepath.Join(c.Root, "_bin")
}

type MakeBinConfig struct {
	Dir string
	Bin map[string]string
}

type Spec = MakeConfig

func MakeBin(cfg MakeBinConfig) error {
	start := time.Now()
	log.Tracef("MakeBin %#v", cfg)
	defer func() {
		log.Tracef("MakeBin %v took %v", cfg.Dir, time.Since(start))
	}()

	_ = os.RemoveAll(cfg.Dir)

	err := os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		return fmt.Errorf("MakeBin: %w", err)
	}

	for name, p := range cfg.Bin {
		log.Tracef("MakeBin linking %v: %v", name, p)

		_, err := os.Stat(p)
		if err != nil {
			return fmt.Errorf("MakeBin: %w", err)
		}

		err = os.Symlink(p, filepath.Join(cfg.Dir, name))
		if err != nil {
			return fmt.Errorf("MakeBin: %w", err)
		}
	}

	return nil
}

func Make(ctx context.Context, cfg MakeConfig) (Spec, error) {
	start := time.Now()
	log.Tracef("Make %v", cfg.Root)
	defer func() {
		log.Tracef("Make %v took %v", cfg.Root, time.Since(start))
	}()

	err := os.RemoveAll(cfg.Root)
	if err != nil {
		return Spec{}, err
	}

	err = os.MkdirAll(cfg.Root, os.ModePerm)
	if err != nil {
		return Spec{}, err
	}

	err = os.MkdirAll(cfg.Dir(), os.ModePerm)
	if err != nil {
		return Spec{}, err
	}

	err = MakeBin(MakeBinConfig{
		Dir: cfg.BinDir(),
		Bin: cfg.Bin,
	})
	if err != nil {
		return Spec{}, err
	}

	for _, file := range cfg.Src {
		if err := ctx.Err(); err != nil {
			return Spec{}, err
		}

		err := utils.Cp(file.From, filepath.Join(cfg.Dir(), file.To))
		if err != nil {
			return Spec{}, fmt.Errorf("make: %w", err)
		}
	}

	for _, tarFile := range cfg.SrcTar {
		err := utils.Untar(ctx, tarFile, cfg.Dir())
		if err != nil {
			return Spec{}, fmt.Errorf("make: untar: %w", err)
		}
	}

	return cfg, nil
}

type IOConfig struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer
}

type ExecConfig struct {
	IOConfig
	Context context.Context
	BinDir  string
	Dir     string
	Cmd     string
	Env     map[string]string
}

func Exec(cfg ExecConfig, isolatePath bool) *exec.Cmd {
	log.Tracef("Exec: %v %v", cfg.Cmd, cfg.Dir)

	err := os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		panic(err)
	}

	cmd := exec.CommandContext(cfg.Context, "bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", cfg.Cmd)
	cmd.Dir = cfg.Dir

	cmd.Stdin = cfg.Stdin
	cmd.Stdout = cfg.Stdout
	cmd.Stderr = cfg.Stderr

	pathStr := func(path string) string {
		if isolatePath {
			return "PATH=/usr/sbin:/usr/bin:/sbin:/bin:" + cfg.BinDir
		} else {
			return "PATH=" + cfg.BinDir + ":" + path
		}
	}

	cmd.Env = make([]string, 0)
	pathReplaced := false
	for k, v := range cfg.Env {
		if k == "PATH" {
			pathReplaced = true
			cmd.Env = append(cmd.Env, pathStr(v))
			continue
		}

		cmd.Env = append(cmd.Env, fmt.Sprintf("%v=%v", k, v))
	}

	if !pathReplaced {
		cmd.Env = append(cmd.Env, pathStr(os.Getenv("PATH")))
	}

	return cmd
}
