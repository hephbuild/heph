package hlocks

import "errors"

type Multi struct {
	ls []func() error
}

func NewMulti() *Multi {
	return &Multi{}
}

func (m *Multi) Add(l func() error) {
	m.ls = append(m.ls, l)
}

func (m *Multi) UnlockAll() error {
	var errs error
	for _, l := range m.ls {
		err := l()
		if err != nil {
			errs = errors.Join(errs, err)
		}
	}

	return errs
}
