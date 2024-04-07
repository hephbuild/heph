package worker2

import (
	"errors"
	"fmt"
	"go.uber.org/multierr"
)

type Error struct {
	ID    uint64
	Name  string
	State ExecState
	Err   error

	root error
}

func (e Error) Skipped() bool {
	return e.State == ExecStateSkipped
}

func CollectUniqueErrors(inErrs []error) []error {
	var errs []error
	jerrs := map[uint64]Error{}

	for _, err := range inErrs {
		var jerr Error
		if errors.As(err, &jerr) {
			jerrs[jerr.ID] = jerr
		} else {
			errs = append(errs, err)
		}
	}

	for _, err := range jerrs {
		errs = append(errs, err)
	}

	return errs
}

func CollectRootErrors(err error) error {
	errs := make([]error, 0)

	for _, err := range multierr.Errors(err) {
		var jerr Error
		if errors.As(err, &jerr) {
			errs = append(errs, jerr.Root())
		} else {
			errs = append(errs, err)
		}
	}

	return multierr.Combine(CollectUniqueErrors(errs)...)
}

func (e Error) Root() error {
	if e.root != nil {
		return e.root
	}

	var roots []error
	for _, err := range multierr.Errors(e.Err) {
		var jerr Error
		if errors.As(err, &jerr) {
			if jerr.Skipped() {
				roots = append(roots, jerr.Root())
			}
		}
	}

	if len(roots) == 0 {
		e.root = e
		return e
	}

	e.root = multierr.Combine(roots...)
	return e.root
}

func (e Error) Unwrap() error {
	return e.Err
}

func (e Error) Error() string {
	return fmt.Sprintf("%v: %v: %v", e.Name, e.State, e.Err)
}
