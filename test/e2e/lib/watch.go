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

func Watch(tgt string, o RunOpts) (*Watcher, error) {
	args := []string{"watch", tgt, "--plain"}
	for k, v := range o.Params {
		args = append(args, "-p", k+"="+v)
	}
	cmd := exec.Command("heph", args...)
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
		if err != nil {
			panic(err)
		}
		if !w.expectedKill {
			panic("watch finished unexpectedly")
		}
	}()

	return w, nil
}
