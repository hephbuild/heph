package xerrors

import "errors"

func As[T error](err error) (T, bool) {
	var asErr T
	ok := errors.As(err, &asErr)
	return asErr, ok
}
