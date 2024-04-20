package xtea

import (
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"runtime/debug"
	"sync"
)

var tuim sync.Mutex

const debugSingleflight = false

var tuiStack []byte

func SingleflightTry() bool {
	if !tuim.TryLock() {
		if debugSingleflight {
			panic(fmt.Sprintf("concurrent call of poolui.Wait, already running at:\n%s\ntrying to run at", tuiStack))
		}
		return false
	}

	if debugSingleflight {
		tuiStack = debug.Stack()
	}

	return true
}

func SingleflightDone() {
	if debugSingleflight {
		tuiStack = nil
	}
	tuim.Unlock()
}

var resetTermFunc func()

func SetResetTerminal(p *tea.Program) {
	if p == nil {
		resetTermFunc = nil
		return
	}

	resetTermFunc = func() {
		_ = p.ReleaseTerminal()
		_ = p.RestoreTerminal()
	}
}

func ResetTerminal() {
	if f := resetTermFunc; f != nil {
		f()
	}
}
