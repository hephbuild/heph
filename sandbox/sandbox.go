package sandbox

import (
	"context"
	"fmt"
	"heph/log"
	"heph/utils"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
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
		log.Debugf("MakeBin %v took %v", cfg.Dir, time.Since(start))
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
		log.Debugf("Make %v took %v", cfg.Root, time.Since(start))
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

func BashArgs(cmds []string) []string {
	args := []string{"bash", "--noprofile", "--norc", "-e", "-u", "-o", "pipefail"}
	//args = append(args, "-x")
	args = append(args, "-c", strings.Join(cmds, "\n"))

	return args
}

func ExecArgs(args []string, env map[string]string) ([]string, error) {
	for i, arg := range args {
		if strings.HasPrefix(arg, "$") {
			v, ok := env[strings.TrimPrefix(arg, "$")]
			if !ok {
				return nil, fmt.Errorf("%v is unbound", arg)
			}
			args[i] = v
		}
	}

	return args, nil
}

type ExecConfig struct {
	IOConfig
	Context  context.Context
	BinDir   string
	Dir      string
	Env      map[string]string
	ExecArgs []string
	CmdArgs  []string
}

func Exec(cfg ExecConfig, isolatePath bool) *exec.Cmd {
	args := cfg.ExecArgs
	args = append(args, cfg.CmdArgs...)

	log.Tracef("Exec in %v: %v", cfg.Dir, args)

	err := os.MkdirAll(cfg.Dir, os.ModePerm)
	if err != nil {
		panic(err)
	}

	cmd := exec.CommandContext(cfg.Context, args[0], args[1:]...)
	cmd.Dir = cfg.Dir

	cmd.Stdin = cfg.Stdin
	cmd.Stdout = cfg.Stdout
	cmd.Stderr = cfg.Stderr

	pathStr := func(path string) string {
		if isolatePath {
			return "PATH=" + cfg.BinDir + ":/usr/sbin:/usr/bin:/sbin:/bin"
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

	cmd.SysProcAttr = sysProcAttr()

	return cmd
}
