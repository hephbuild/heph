package sandbox

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils/fs"
	"heph/utils/tar"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

type MakeConfig struct {
	Dir    string
	BinDir string
	Bin    map[string]string
	Src    []tar.TarFile
	SrcTar []string
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
		log.Debugf("MakeBin %v took %v", cfg.Dir, time.Since(start))
	}()

	_ = os.RemoveAll(cfg.Dir)

	err := os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		return fmt.Errorf("MakeBin: %w", err)
	}

	for name, p := range cfg.Bin {
		if name == "" {
			panic("bin name cannot be empty: " + p)
		}

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

func Make(ctx context.Context, cfg MakeConfig) error {
	start := time.Now()
	log.Tracef("Make %v", cfg.Dir)
	defer func() {
		log.Debugf("Make %v took %v", cfg.Dir, time.Since(start))
	}()

	err := os.RemoveAll(cfg.Dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		return err
	}

	err = MakeBin(MakeBinConfig{
		Dir: cfg.BinDir,
		Bin: cfg.Bin,
	})
	if err != nil {
		return err
	}

	for _, file := range cfg.Src {
		if err := ctx.Err(); err != nil {
			return err
		}

		err := fs.Cp(file.From, filepath.Join(cfg.Dir, file.To))
		if err != nil {
			return fmt.Errorf("make: %w", err)
		}
	}

	for _, tarFile := range cfg.SrcTar {
		err := tar.Untar(ctx, tarFile, cfg.Dir, false)
		if err != nil {
			return fmt.Errorf("make: untar: %w", err)
		}
	}

	return nil
}

type IOConfig struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer
}

type ExecConfig struct {
	IOConfig
	Context  context.Context
	BinDir   string
	Dir      string
	Env      map[string]string
	ExecArgs []string
}

func AddPathEnv(env map[string]string, binDir string, isolatePath bool) {
	pathStr := func(path string) string {
		if isolatePath {
			return binDir + ":/usr/sbin:/usr/bin:/sbin:/bin"
		} else {
			return binDir + ":" + path
		}
	}

	if v, ok := env["PATH"]; ok {
		env["PATH"] = pathStr(v)
	} else {
		env["PATH"] = pathStr(os.Getenv("PATH"))
	}
}

func envMapToArray(m map[string]string) []string {
	env := make([]string, 0, len(m))
	for k, v := range m {
		env = append(env, fmt.Sprintf("%v=%v", k, v))
	}
	return env
}

func Exec(cfg ExecConfig) *exec.Cmd {
	args := cfg.ExecArgs

	log.Tracef("Exec in %v: %v", cfg.Dir, args)

	err := os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		panic(err)
	}

	cmd := exec.CommandContext(cfg.Context, args[0], args[1:]...)
	cmd.Dir = cfg.Dir

	cmd.Env = envMapToArray(cfg.Env)
	cmd.Stdin = cfg.Stdin
	cmd.Stdout = cfg.Stdout
	cmd.Stderr = cfg.Stderr

	cmd.SysProcAttr = sysProcAttr()

	// Attaching to stdin or stdout doesn't seem to work with setPgid
	if cmd.Stdin == os.Stdin || cmd.Stdout == os.Stdout || cmd.Stderr == os.Stderr {
		cmd.SysProcAttr.Setpgid = false
	}

	return cmd
}
