package platform

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"fmt"
	"github.com/mattn/go-isatty"
	"github.com/muesli/termenv"
	"os"
	"os/exec"
	"os/user"
	"strings"
)

type dockerExecutor struct {
	arch      string
	os        string
	local     Executor
	platform  string
	exe       string
	image     string
	mountsMap map[string]string
	args      []string
}

func (d *dockerExecutor) Os() string {
	return d.os
}

func (d *dockerExecutor) Arch() string {
	return d.arch
}

func (d *dockerExecutor) remap(p string) string {
	for cpath, hpath := range d.mountsMap {
		if strings.HasPrefix(p, cpath) {
			return strings.Replace(p, cpath, hpath, 1)
		}
	}

	return p
}

func (d *dockerExecutor) Exec(ctx context.Context, o ExecOptions, execArgs []string) error {
	dockerArgs := []string{d.exe, "run", "--rm"}
	for k, v := range o.Env {
		dockerArgs = append(dockerArgs, "-e", k+"="+v)
	}
	o.Env = nil // Prevent from duplicating envs in exec, since they are transferred through -e

	if f, ok := o.IOCfg.Stdin.(termenv.File); ok {
		if isatty.IsTerminal(f.Fd()) {
			dockerArgs = append(dockerArgs, "-it")
		}
	}

	u, _ := user.Current()
	if u != nil {
		dockerArgs = append(dockerArgs, "--user="+u.Uid+":"+u.Gid)
	}

	for _, dir := range []string{o.HomeDir, o.BinDir, o.WorkDir} {
		hdir := d.remap(dir)
		cdir := dir
		dockerArgs = append(dockerArgs, "--volume="+hdir+":"+cdir+":Z")
	}

	dockerArgs = append(dockerArgs, "--workdir="+o.WorkDir)
	dockerArgs = append(dockerArgs, "--platform="+d.platform)

	dockerArgs = append(dockerArgs, d.args...)

	dockerArgs = append(dockerArgs, d.image)
	dockerArgs = append(dockerArgs, execArgs...)

	return d.local.Exec(ctx, o, dockerArgs)
}

func NewDockerExecutor(exe, os, arch, image string, mountsMap map[string]string, args []string) Executor {
	return &dockerExecutor{
		os:        os,
		arch:      arch,
		local:     NewLocalExecutor(),
		platform:  os + "/" + arch,
		exe:       exe,
		image:     image,
		mountsMap: mountsMap,
		args:      args,
	}
}

type dockerProvider struct {
	name      string
	os, arch  string
	exe       string
	mountsMap map[string]string
}

func (p dockerProvider) NewExecutor(labels map[string]string, options map[string]interface{}) (Executor, error) {
	if !HasAllLabels(labels, map[string]string{
		"name": p.name,
		"os":   p.os,
		"arch": p.arch,
	}) {
		return nil, nil
	}

	image, _ := options["image"].(string)
	if image == "" {
		return nil, fmt.Errorf("image option missing")
	}

	args, _ := options["args"].(string)

	return NewDockerExecutor(p.exe, p.os, p.arch, image, p.mountsMap, strings.Fields(args)), nil
}

type dockerInspectData struct {
	HostConfig struct {
		Binds []string
	}
}

func dockerInspect(exe string) ([]dockerInspectData, error) {
	hostnameb, err := os.ReadFile("/etc/hostname")
	if err != nil {
		return nil, err
	}

	cmd := exec.Command(exe, "inspect", strings.TrimSpace(string(hostnameb)))
	b, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("%w: %s", err, bytes.TrimSpace(b))
	}

	var data []dockerInspectData
	err = json.Unmarshal(b, &data)
	if err != nil {
		return nil, fmt.Errorf("%w: %s", err, bytes.TrimSpace(b))
	}

	return data, nil
}

type dockerVersionData struct {
	Os            string
	Arch          string
	KernelVersion string
}

func dockerVersion(exe string) (*dockerVersionData, error) {
	cmd := exec.Command(exe, "version", "--format", "{{json .Server}}")
	b, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("%w: %s", err, bytes.TrimSpace(b))
	}

	var data dockerVersionData
	err = json.Unmarshal(b, &data)
	if err != nil {
		return nil, fmt.Errorf("%w: %s", err, bytes.TrimSpace(b))
	}

	return &data, nil
}

func NewDockerProvider(name string, options map[string]interface{}) (Provider, error) {
	exe, err := exec.LookPath("docker")
	if err != nil {
		return nil, err
	}

	versionData, err := dockerVersion(exe)
	if err != nil {
		return nil, fmt.Errorf("version: %w", err)
	}

	goos := versionData.Os
	goarch := versionData.Arch
	if goarch == "aarch64" {
		goarch = "arm64"
	}

	// Docker Out Of Docker; basically mounting the docker socket in a docker container
	// This requires remapping the binds
	mountsMap := map[string]string{}
	if v, _ := options["dood"].(bool); v {
		data, err := dockerInspect(exe)
		if err != nil {
			return nil, fmt.Errorf("inspect: %w", err)
		}

		if len(data) > 0 {
			for _, bind := range data[0].HostConfig.Binds {
				parts := strings.Split(bind, ":")
				if len(parts) >= 2 {
					host := parts[0]
					container := parts[1]
					mountsMap[container] = host
				}
			}
		}
	}

	return &dockerProvider{
		name:      name,
		os:        goos,
		arch:      goarch,
		exe:       exe,
		mountsMap: mountsMap,
	}, nil
}

func init() {
	RegisterProvider("docker", func(name string, options map[string]interface{}) (Provider, error) {
		return NewDockerProvider(name, options)
	})
}
