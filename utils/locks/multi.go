package locks

import "go.uber.org/multierr"

type Multi struct {
	unlocks []UnlockFunc
}

type UnlockFunc = func() error

func (m *Multi) Add(l Locker) {
	m.AddFunc(l.Unlock)
}

func (m *Multi) AddFunc(f UnlockFunc) {
	// Prepend so that we unlock in reverse order
	m.unlocks = append([]UnlockFunc{f}, m.unlocks...)
}

func (m *Multi) Unlock() error {
	var errs error

	for _, unlock := range m.unlocks {
		err := unlock()
		if err != nil {
			errs = multierr.Append(errs, err)
		}
	}

	return errs
}
