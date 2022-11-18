package worker

import (
	"errors"
	"fmt"
	"go.uber.org/multierr"
)

type JobError struct {
	ID    uint64
	Name  string
	State JobState
	Err   error

	root error
}

func (e JobError) Skipped() bool {
	return e.State == StateSkipped
}

func CollectUniqueErrors(inErrs []error) []error {
	var errs []error
	jerrs := map[uint64]JobError{}

	for _, err := range inErrs {
		var jerr JobError
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
		var jerr JobError
		if errors.As(err, &jerr) {
			errs = append(errs, jerr.Root())
		} else {
			errs = append(errs, err)
		}
	}

	return multierr.Combine(CollectUniqueErrors(errs)...)
}

func (e JobError) Root() error {
	if e.root != nil {
		return e.root
	}

	var err error = e
	for {
		var jerr JobError
		if errors.As(err, &jerr) {
			if jerr.Skipped() {
				if _, ok := jerr.Err.(JobError); !ok {
					break
				}

				err = jerr.Err
				continue
			}
		}

		break
	}

	e.root = err
	return err
}

func (e JobError) Unwrap() error {
	return e.Err
}

func (e JobError) Error() string {
	return fmt.Sprintf("%v is %v: %v", e.Name, e.State, e.Err)
}
