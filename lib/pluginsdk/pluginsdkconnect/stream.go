package pluginsdkconnect

import (
	"connectrpc.com/connect"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"google.golang.org/protobuf/types/known/structpb"
)

var _ pluginsdk.HandlerStreamReceive[*structpb.Value] = (*connectHandlerStreamReceive[structpb.Value, *structpb.Value])(nil)

type connectHandlerStreamReceive[T any, TP *T] struct {
	strm *connect.ServerStreamForClient[T]
}

func (c connectHandlerStreamReceive[T, TP]) Receive() bool {
	return c.strm.Receive()
}

func (c connectHandlerStreamReceive[T, TP]) Msg() *T {
	return c.strm.Msg()
}

func (c connectHandlerStreamReceive[T, TP]) Err() error {
	return c.strm.Err()
}

func (c connectHandlerStreamReceive[T, TP]) CloseReceive() error {
	return c.strm.Close()
}

func connectServerStream[T any](strm *connect.ServerStreamForClient[T]) pluginsdk.HandlerStreamReceive[*T] {
	return connectHandlerStreamReceive[T, *T]{
		strm: strm,
	}
}
