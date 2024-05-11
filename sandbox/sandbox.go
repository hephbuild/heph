package sandbox

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xmath"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"time"
)

type File = tar.File

type TarFile struct {
	Path string
	Size int64
}

type MakeConfig struct {
	Dir           string
	BinDir        string
	Bin           map[string]string
	Files         []File
	FilesTar      []TarFile
	LinkFiles     []File
	ProgressFiles func(percent float64)
	ProgressTars  func(percent float64)
	ProgressLinks func(percent float64)
}

type MakeBinConfig struct {
	Dir string
	Bin map[string]string
}

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

	xfs.MakeDirsReadWrite(cfg.Dir)

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

	for i, file := range cfg.Files {
		if err := ctx.Err(); err != nil {
			return err
		}

		if cfg.ProgressFiles != nil {
			percent := math.Round(xmath.Percent(i, len(cfg.Files)))

			cfg.ProgressFiles(percent)
		}

		to := filepath.Join(cfg.Dir, file.To)

		if untarDedup.Has(to) {
			continue
		}
		untarDedup.Add(to)

		err := xfs.Cp(ctx, file.From, to)
		if err != nil {
			return fmt.Errorf("make: %w", err)
		}

		if cfg.ProgressFiles != nil {
			percent := math.Round(xmath.Percent(i+1, len(cfg.Files)))

			cfg.ProgressFiles(percent)
		}
	}

	tarSizeSum := ads.Reduce(cfg.FilesTar, func(s int64, file TarFile) int64 {
		// Do not show partial (and wrong) percentage
		if s < 0 {
			return -1
		}
		if file.Size == 0 {
			return -1
		}
		return s + file.Size
	}, int64(0))
	var tarSizeWritten int64

	var progress func(written int64)
	if cfg.ProgressTars != nil {
		if tarSizeSum > 0 {
			progress = func(written int64) {
				percent := math.Round(xmath.Percent(written+tarSizeWritten, tarSizeSum))

				cfg.ProgressTars(percent)
			}

			progress(0)
		} else {
			cfg.ProgressTars(-1)
		}
	}

	for _, tarFile := range cfg.FilesTar {
		done := log.TraceTimingDone("untar " + tarFile.Path)
		err := tar.UntarPath(ctx, tarFile.Path, cfg.Dir, tar.UntarOptions{
			Dedup:    untarDedup,
			Progress: progress,
		})
		if err != nil {
			return fmt.Errorf("make: %w", err)
		}
		done()

		tarSizeWritten += tarFile.Size

		if progress != nil {
			progress(tarSizeWritten)
		}
	}

	for i, file := range cfg.LinkFiles {
		if err := ctx.Err(); err != nil {
			return err
		}

		if cfg.ProgressLinks != nil {
			percent := math.Round(xmath.Percent(i, len(cfg.LinkFiles)))

			cfg.ProgressLinks(percent)
		}

		to := filepath.Join(cfg.Dir, file.To)

		if untarDedup.Has(to) {
			continue
		}
		untarDedup.Add(to)

		err := xfs.CreateParentDir(to)
		if err != nil {
			return err
		}

		err = os.Link(file.From, to)
		if err != nil {
			return fmt.Errorf("make: link %v to %v: %w", file.From, to, err)
		}

		if cfg.ProgressLinks != nil {
			percent := math.Round(xmath.Percent(i+1, len(cfg.LinkFiles)))

			cfg.ProgressLinks(percent)
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
	SoftContext context.Context
	Context     context.Context
	BinDir      string
	Dir         string
	Env         map[string]string
	ExecArgs    []string
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

type Cmd struct {
	*exec.Cmd
	SoftContext context.Context
}

func (c *Cmd) Start() error {
	panic("not implemented")
}

func (c *Cmd) watchSoftContext() {
	<-c.SoftContext.Done()
	p := c.Cmd.Process
	if p == nil {
		return
	}
	_ = p.Signal(syscall.SIGINT)
}

func (c *Cmd) Run() error {
	go c.watchSoftContext()

	err := c.Cmd.Run()
	if cerr := c.SoftContext.Err(); cerr != nil {
		if err != nil {
			err = fmt.Errorf("%v: %w", err, cerr)
		} else {
			err = cerr
		}
	}

	return err
}

func Exec(cfg ExecConfig) *Cmd {
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

	return &Cmd{cmd, cfg.SoftContext}
}
