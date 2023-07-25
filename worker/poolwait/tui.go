package poolwait

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolui"
	"os"
	"sync"
)

var tuim sync.Mutex

//var tuiStack []byte

func runProgram(p *tea.Program) error {
	defer func() {
		log.SetDiversion(nil)
		_ = p.ReleaseTerminal()
	}()

	_, err := p.Run()
	return err
}

func termUI(ctx context.Context, name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	if !tuim.TryLock() {
		//panic(fmt.Sprintf("concurrent call of poolui.Wait, already running at:\n%s\ntrying to run at", stack))
		return logUI(name, deps, pool)
	}

	//tuiStack = debug.Stack()

	defer func() {
		//tuiStack = nil
		tuim.Unlock()
	}()

	r := poolui.New(ctx, name, deps, pool, true)

	p := tea.NewProgram(r, tea.WithOutput(os.Stderr), tea.WithoutSignalHandler())

	err := runProgram(p)
	if err != nil {
		return err
	}

	if !deps.IsDone() {
		pool.Stop(fmt.Errorf("TUI exited unexpectedly"))
	}

	return nil
}
