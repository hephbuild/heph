package lib

import (
	"os"
	"os/exec"
)

type Watcher struct {
	Cmd          *exec.Cmd
	expectedKill bool
}

func (w *Watcher) Kill() error {
	w.expectedKill = true
	err := w.Cmd.Process.Kill()
	if err != nil {
		return err
	}

	return nil
}

func Watch(tgt string) (*Watcher, error) {
	return WatchO(tgt, defaultOpts)
}

func WatchO(tgt string, o RunOpts) (*Watcher, error) {
	cmd := commandO(o, "watch", tgt, "--plain")
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	err := cmd.Start()
	if err != nil {
		return nil, err
	}

	w := &Watcher{
		Cmd: cmd,
	}

	go func() {
		err := cmd.Wait()
		if !w.expectedKill {
			if err != nil {
				panic(err)
			}
			panic("watch finished unexpectedly")
		}
	}()

	return w, nil
}
