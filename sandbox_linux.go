package main

import (
	"fmt"
	"github.com/opencontainers/runc/libcontainer"
	"github.com/opencontainers/runc/libcontainer/configs"
	_ "github.com/opencontainers/runc/libcontainer/nsenter"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-spec/specs-go"
	"github.com/sirupsen/logrus"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
)

func init() {
	if len(os.Args) > 1 && os.Args[1] == "sandbox-init" {
		runtime.GOMAXPROCS(1)
		runtime.LockOSThread()
		factory, err := libcontainer.New("")
		if err != nil {
			panic(err)
		}
		if err := factory.StartInitialization(); err != nil {
			logrus.Fatal(err)
		}
		panic("--this line should have never been executed, congratulations--")
	}
}

// from github.com/opencontainers/runc@v1.1.4/utils_linux.go
func newProcess(p specs.Process) (*libcontainer.Process, error) {
	lp := &libcontainer.Process{
		Args: p.Args,
		Env:  p.Env,
		// TODO: fix libcontainer's API to better support uid/gid in a typesafe way.
		User:            fmt.Sprintf("%d:%d", p.User.UID, p.User.GID),
		Cwd:             p.Cwd,
		Label:           p.SelinuxLabel,
		NoNewPrivileges: &p.NoNewPrivileges,
		AppArmorProfile: p.ApparmorProfile,
	}

	if p.ConsoleSize != nil {
		lp.ConsoleWidth = uint16(p.ConsoleSize.Width)
		lp.ConsoleHeight = uint16(p.ConsoleSize.Height)
	}

	if p.Capabilities != nil {
		lp.Capabilities = &configs.Capabilities{}
		lp.Capabilities.Bounding = p.Capabilities.Bounding
		lp.Capabilities.Effective = p.Capabilities.Effective
		lp.Capabilities.Inheritable = p.Capabilities.Inheritable
		lp.Capabilities.Permitted = p.Capabilities.Permitted
		lp.Capabilities.Ambient = p.Capabilities.Ambient
	}
	for _, gid := range p.User.AdditionalGids {
		lp.AdditionalGroups = append(lp.AdditionalGroups, strconv.FormatUint(uint64(gid), 10))
	}
	//for _, rlimit := range p.Rlimits {
	//	rl, err := createLibContainerRlimit(rlimit)
	//	if err != nil {
	//		return nil, err
	//	}
	//	lp.Rlimits = append(lp.Rlimits, rl)
	//}
	return lp, nil
}

func mainSandbox() {
	factory, err := libcontainer.New("/tmp/heph-container", libcontainer.InitArgs(os.Args[0], "sandbox-init"))
	if err != nil {
		logrus.Fatal(err)
		return
	}

	wd, _ := os.Getwd()

	exe, _ := os.Executable()
	exeDir := filepath.Dir(exe)

	spec := specconv.Example()
	spec.Root.Path = "/tmp/py3"
	spec.Mounts = append(spec.Mounts, specs.Mount{
		Destination: wd,
		Type:        "none",
		Source:      wd,
		Options:     []string{"bind"},
	})
	//spec.Mounts = append(spec.Mounts,  specs.Mount{
	//	Destination: "/lib",
	//	Type:        "none",
	//	Source:      "/tmp/mylib",
	//	Options:     []string{"bind"},
	//})
	spec.Process.Cwd = wd
	spec.Process.Env = os.Environ()
	for i, e := range spec.Process.Env {
		if strings.HasPrefix(e, "PATH=") {
			spec.Process.Env[i] = e + ":" + exeDir
			break
		}
	}
	specconv.ToRootless(spec)

	opts := &specconv.CreateOpts{
		CgroupName:       "some-container",
		UseSystemdCgroup: false,
		Spec:             spec,
		RootlessEUID:     true,
		RootlessCgroups:  true,
	}

	config, err := specconv.CreateLibcontainerConfig(opts)
	if err != nil {
		logrus.Fatal(err)
		return
	}

	logrus.Printf("Create")
	container, err := factory.Create("some-container", config)
	if err != nil {
		logrus.Fatal(err)
		return
	}

	cleanup := func() {
		container.Signal(os.Kill, true)
		container.Destroy()
	}
	defer cleanup()

	c := make(chan os.Signal)
	signal.Notify(c, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-c
		cleanup()
	}()

	logrus.Printf("Run")

	process, err := newProcess(*spec.Process)
	if err != nil {
		logrus.Fatal(err)
		return
	}
	process.Stdin = os.Stdin
	process.Stdout = os.Stdout
	process.Stderr = os.Stderr
	process.Init = true

	err = container.Run(process)
	if err != nil {
		logrus.Fatal(err)
		return
	}

	logrus.Printf("Wait")

	// wait for the process to finish.
	_, err = process.Wait()
	if err != nil {
		logrus.Fatal(err)
	}

	logrus.Printf("Destroy")

	// destroy the container.
}
