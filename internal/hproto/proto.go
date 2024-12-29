package hproto

import "google.golang.org/protobuf/proto"

func Clone[T proto.Message](m T) T {
	return proto.Clone(m).(T) //nolint:errcheck
}
