package hkvproto

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/hephbuild/heph/lib/hkv"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/mock"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"
)

type mockBytes struct {
	mock.Mock
}

func (m *mockBytes) Get(ctx context.Context, key string) ([]byte, map[string]string, bool, error) {
	args := m.Called(ctx, key)
	return args.Get(0).([]byte), args.Get(1).(map[string]string), args.Bool(2), args.Error(3)
}

func (m *mockBytes) Exists(ctx context.Context, key string) (bool, error) {
	args := m.Called(ctx, key)
	return args.Bool(0), args.Error(1)
}

func (m *mockBytes) Set(ctx context.Context, key string, value []byte, metadata map[string]string, ttl time.Duration) error {
	args := m.Called(ctx, key, value, metadata, ttl)
	return args.Error(0)
}

func (m *mockBytes) Delete(ctx context.Context, key string) error {
	args := m.Called(ctx, key)
	return args.Error(0)
}

func TestKV_Get(t *testing.T) {
	ctx := context.Background()
	m := new(mockBytes)
	p := hkv.NewT(m, New[*structpb.Value]())

	t.Run("success", func(t *testing.T) {
		val := structpb.NewStringValue("hello")
		data, _ := proto.Marshal(val)
		meta := map[string]string{"foo": "bar"}

		m.On("Get", ctx, "key1").Return(data, meta, true, nil).Once()

		res, resMeta, ok, err := p.Get(ctx, "key1")
		require.NoError(t, err)
		assert.True(t, ok)
		assert.Equal(t, meta, resMeta)
		assert.True(t, proto.Equal(val, res))
		m.AssertExpectations(t)
	})

	t.Run("not found", func(t *testing.T) {
		m.On("Get", ctx, "key2").Return([]byte(nil), map[string]string(nil), false, nil).Once()

		res, resMeta, ok, err := p.Get(ctx, "key2")
		require.NoError(t, err)
		assert.False(t, ok)
		assert.Nil(t, resMeta)
		assert.Nil(t, res)
		m.AssertExpectations(t)
	})

	t.Run("error", func(t *testing.T) {
		m.On("Get", ctx, "key3").Return([]byte(nil), map[string]string(nil), false, errors.New("some error")).Once()

		res, resMeta, ok, err := p.Get(ctx, "key3")
		require.Error(t, err)
		assert.False(t, ok)
		assert.Nil(t, resMeta)
		assert.Nil(t, res)
		m.AssertExpectations(t)
	})

	t.Run("unmarshal error", func(t *testing.T) {
		m.On("Get", ctx, "key4").Return([]byte("invalid data"), map[string]string(nil), true, nil).Once()

		res, resMeta, ok, err := p.Get(ctx, "key4")
		require.ErrorContains(t, err, "unmarshal")
		assert.False(t, ok)
		assert.Nil(t, resMeta)
		assert.Nil(t, res)
		m.AssertExpectations(t)
	})
}

func TestKV_Exists(t *testing.T) {
	ctx := context.Background()
	m := new(mockBytes)
	p := hkv.NewT(m, New[*structpb.Value]())

	m.On("Exists", ctx, "key1").Return(true, nil).Once()
	ok, err := p.Exists(ctx, "key1")
	require.NoError(t, err)
	assert.True(t, ok)

	m.On("Exists", ctx, "key2").Return(false, nil).Once()
	ok, err = p.Exists(ctx, "key2")
	require.NoError(t, err)
	assert.False(t, ok)

	m.AssertExpectations(t)
}

func TestKV_Set(t *testing.T) {
	ctx := context.Background()
	m := new(mockBytes)
	p := hkv.NewT(m, New[*structpb.Value]())

	t.Run("success", func(t *testing.T) {
		val := structpb.NewStringValue("hello")
		data, _ := proto.Marshal(val)
		meta := map[string]string{"foo": "bar"}
		ttl := time.Hour

		m.On("Set", ctx, "key1", data, meta, ttl).Return(nil).Once()

		err := p.Set(ctx, "key1", val, meta, ttl)
		require.NoError(t, err)
		m.AssertExpectations(t)
	})

	t.Run("error", func(t *testing.T) {
		val := structpb.NewStringValue("hello")
		data, _ := proto.Marshal(val)

		m.On("Set", ctx, "key2", data, mock.Anything, mock.Anything).Return(errors.New("some error")).Once()

		err := p.Set(ctx, "key2", val, nil, 0)
		assert.Error(t, err)
		m.AssertExpectations(t)
	})

	t.Run("nil", func(t *testing.T) {
		m.On("Set", ctx, "key3", []byte(nil), mock.Anything, mock.Anything).Return(nil).Once()
		m.On("Get", ctx, "key3").Return([]byte(nil), map[string]string(nil), true, nil).Once()

		err := p.Set(ctx, "key3", nil, nil, 0)
		require.NoError(t, err)

		res, _, ok, err := p.Get(ctx, "key3")
		require.NoError(t, err)
		assert.True(t, ok)
		assert.Nil(t, res)
		m.AssertExpectations(t)
	})
}

func TestKV_Delete(t *testing.T) {
	ctx := context.Background()
	m := new(mockBytes)
	p := hkv.NewT(m, New[*structpb.Value]())

	m.On("Delete", ctx, "key1").Return(nil).Once()
	err := p.Delete(ctx, "key1")
	require.NoError(t, err)

	m.On("Delete", ctx, "key2").Return(errors.New("some error")).Once()
	err = p.Delete(ctx, "key2")
	require.Error(t, err)

	m.AssertExpectations(t)
}
