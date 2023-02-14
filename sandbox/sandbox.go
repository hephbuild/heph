package sandbox

import (
	"context"
	"fmt"
	log "heph/hlog"
	"heph/utils"
	"heph/utils/fs"
	"heph/utils/sets"
	"heph/utils/tar"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

type MakeConfig struct {
	Dir       string
	BinDir    string
	Bin       map[string]string
	Files     []tar.TarFile
	FilesTar  []string
	LinkFiles []tar.TarFile
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

	// TODO: figure out if different origins would override each other
	untarDedup := sets.NewStringSet(0)

	for _, file := range cfg.Files {
		if err := ctx.Err(); err != nil {
			return err
		}

		to := filepath.Join(cfg.Dir, file.To)

		if untarDedup.Has(to) {
			continue
		}
		untarDedup.Add(to)

		err := fs.Cp(file.From, to)
		if err != nil {
			return fmt.Errorf("make: %w", err)
		}
	}

	for _, tarFile := range cfg.FilesTar {
		done := utils.TraceTimingDone("untar " + tarFile)
		err := tar.UntarWith(ctx, tarFile, cfg.Dir, tar.UntarOptions{
			List:  false,
			Dedup: untarDedup,
		})
		if err != nil {
			return fmt.Errorf("make: untar: %w", err)
		}
		done()
	}

	for _, file := range cfg.LinkFiles {
		if err := ctx.Err(); err != nil {
			return err
		}

		to := filepath.Join(cfg.Dir, file.To)

		if untarDedup.Has(to) {
			continue
		}
		untarDedup.Add(to)

		err := fs.CreateParentDir(to)
		if err != nil {
			return err
		}

		err = os.Link(file.From, to)
		if err != nil {
			return fmt.Errorf("make: link %v to %v: %w", file.From, to, err)
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
