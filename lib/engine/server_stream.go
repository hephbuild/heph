package engine

import (
	"errors"
	"google.golang.org/protobuf/types/known/structpb"
)

var ErrNotImplemented = errors.New("not implemented")
var ErrNotFound = errors.New("not found")

type HandlerStreamSend[T any] interface {
	Send(T) error
	CloseSend(err error) error
}

type HandlerStreamReceive[T any] interface {
	Receive() bool
	Msg() T
	Err() error
	CloseReceive() error
}

type HandlerStream[T any] interface {
	HandlerStreamReceive[T]
	HandlerStreamSend[T]
}

var _ HandlerStreamReceive[*structpb.Value] = (*ChanHandlerStream[*structpb.Value])(nil)
var _ HandlerStreamSend[*structpb.Value] = (*ChanHandlerStream[*structpb.Value])(nil)

type ChanHandlerStream[T any] struct {
	ch           chan T
	receiveClose chan struct{}
	msg          T
	err          error
}

// Receive

func (c *ChanHandlerStream[T]) Receive() bool {
	msg, ok := <-c.ch
	if !ok {
		return false
	}

	c.msg = msg

	return true
}

func (c *ChanHandlerStream[T]) Msg() T {
	return c.msg
}

func (c *ChanHandlerStream[T]) Err() error {
	return c.err
}

func (c *ChanHandlerStream[T]) CloseReceive() error {
	close(c.receiveClose)

	return nil
}

// Send

func (c *ChanHandlerStream[T]) Send(t T) error {
	select {
	case c.ch <- t:
		return nil
	case <-c.receiveClose:
		return errors.New("receiver gone")
	}
}

func (c *ChanHandlerStream[T]) CloseSend(err error) error {
	c.err = err
	close(c.ch)

	return nil
}

func NewChanHandlerStream[T any]() *ChanHandlerStream[T] {
	return &ChanHandlerStream[T]{
		ch:           make(chan T),
		receiveClose: make(chan struct{}),
	}
}

func NewNoopChanHandlerStream[T any]() *ChanHandlerStream[T] {
	strm := NewChanHandlerStream[T]()
	strm.CloseSend(nil)

	return strm
}

func NewChanHandlerStreamFunc[T any](f func(send func(T) error) error) *ChanHandlerStream[T] {
	strm := NewChanHandlerStream[T]()

	go func() {
		err := f(strm.Send)
		strm.CloseSend(err)
	}()

	return strm
}

var _ HandlerStreamReceive[*structpb.Value] = (*HookHandlerStream[*structpb.Value])(nil)

type HookHandlerStream[T any] struct {
	HandlerStreamReceive[T]

	onCloseReceive func()
	onErr          func(error) error
}

func (h HookHandlerStream[T]) CloseReceive() error {
	if h.onCloseReceive == nil {
		return h.HandlerStreamReceive.CloseReceive()
	}

	defer func() {
		h.onCloseReceive()
	}()

	return h.HandlerStreamReceive.CloseReceive()
}

func (h HookHandlerStream[T]) Err() error {
	if h.onErr == nil {
		return h.HandlerStreamReceive.Err()
	}

	return h.onErr(h.HandlerStreamReceive.Err())
}

func WithOnCloseReceive[T any](r HandlerStreamReceive[T], f func()) HookHandlerStream[T] {
	return HookHandlerStream[T]{
		HandlerStreamReceive: r,
		onCloseReceive:       f,
	}
}

func WithOnErr[T any](r HandlerStreamReceive[T], f func(error) error) HookHandlerStream[T] {
	return HookHandlerStream[T]{
		HandlerStreamReceive: r,
		onErr:                f,
	}
}
