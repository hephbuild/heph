package pluginsdkconnect

import (
	"errors"
	"fmt"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

func NewStackRecursionError(stack string) *connect.Error {
	cerr := connect.NewError(connect.CodeAborted, fmt.Errorf("stack recursion: %v", stack))

	msg, err := connect.NewErrorDetail(&corev1.StackRecursionError{Stack: stack})
	if err != nil {
		panic(err)
	}

	cerr.AddDetail(msg)

	return cerr
}

func IsStackRecursionError(err error) bool {
	_, ok := AsStackRecursionError(err)

	return ok
}

func AsStackRecursionError(err error) (*corev1.StackRecursionError, bool) {
	var cerr *connect.Error
	if errors.As(err, &cerr) {
		for _, detail := range cerr.Details() {
			if detail.Type() != string((&corev1.StackRecursionError{}).ProtoReflect().Descriptor().FullName()) {
				continue
			}

			msg, err := detail.Value()
			if err != nil {
				continue
			}

			serr, ok := msg.(*corev1.StackRecursionError)
			if !ok {
				continue
			}

			return serr, true
		}
	}

	return nil, false
}

func ForwardError(err error) bool {
	return IsStackRecursionError(err)
}
