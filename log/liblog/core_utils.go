package liblog

import (
	"go.uber.org/multierr"
	"sync"
)

type LevelEnablerFunc func(Level) bool

func NewLevelEnabler(core Core, enabler LevelEnablerFunc) Core {
	return levelEnabler{
		Core:    core,
		enabler: enabler,
	}
}

type levelEnabler struct {
	Core
	enabler LevelEnablerFunc
}

func (l levelEnabler) Enabled(lvl Level) bool {
	if !l.enabler(lvl) {
		return false
	}

	return l.Core.Enabled(lvl)
}

var _ Core = (*levelEnabler)(nil)

func NewTee(cores ...Core) Core {
	return tee{
		cores: cores,
	}
}

type tee struct {
	cores []Core
}

func (l tee) Log(entry Entry) error {
	var err error
	for _, core := range l.cores {
		err = multierr.Append(err, core.Log(entry))
	}

	return err
}

func (l tee) Enabled(lvl Level) bool {
	for _, core := range l.cores {
		if core.Enabled(lvl) {
			return true
		}
	}

	return false
}

var _ Core = (*tee)(nil)

func NewLock(core Core) Core {
	return &lock{
		core: core,
	}
}

type lock struct {
	core Core
	m    sync.Mutex
}

func (l *lock) Log(entry Entry) error {
	l.m.Lock()
	defer l.m.Unlock()

	return l.core.Log(entry)
}

func (l *lock) Enabled(lvl Level) bool {
	return l.core.Enabled(lvl)
}

var _ Core = (*lock)(nil)
