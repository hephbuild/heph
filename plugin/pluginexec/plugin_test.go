package pluginexec

import (
	"bytes"
	"connectrpc.com/connect"
	"context"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/hephv2/plugin/hpipe"
	shv1 "github.com/hephbuild/hephv2/plugin/pluginexec/gen/heph/plugin/sh/v1"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"golang.org/x/sync/errgroup"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/types/known/anypb"
	"google.golang.org/protobuf/types/known/structpb"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(sandboxPath)

	p := New()

	{
		res, err := p.Config(ctx, connect.NewRequest(&pluginv1.ConfigRequest{}))
		require.NoError(t, err)

		b, err := protojson.Marshal(res.Msg.TargetSchema)
		require.NoError(t, err)
		require.NotEmpty(t, b)
		//require.JSONEq(t, `{"name":"Target", "field":[{"name":"run", "number":1, "label":"LABEL_REPEATED", "type":"TYPE_STRING", "jsonName":"run"}]}`, string(b))
	}

	var def *pluginv1.TargetDef
	{
		runArg, err := structpb.NewValue([]any{"echo", "hello"})
		require.NoError(t, err)

		res, err := p.Parse(ctx, connect.NewRequest(&pluginv1.ParseRequest{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "some/pkg",
					Name:    "target",
					Driver:  "exec",
				},
				Config: map[string]*structpb.Value{
					"run": runArg,
				},
			},
		}))
		require.NoError(t, err)

		def = res.Msg.Target
	}

	{
		res, err := p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
			Target:      def,
			SandboxPath: sandboxPath,
		}))
		require.NoError(t, err)

		assert.Len(t, res.Msg.Artifacts, 1)
	}
}

func TestPipeStdout(t *testing.T) {
	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(sandboxPath)

	p := New()

	_, rpcHandler := pluginv1connect.NewDriverHandler(p)

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		path, h := p.PipesHandler()
		if strings.HasPrefix(req.URL.Path, path) {
			h.ServeHTTP(w, req)
		} else {
			rpcHandler.ServeHTTP(w, req)
		}
	}))
	defer srv.Close()

	pc := pluginv1connect.NewDriverClient(srv.Client(), srv.URL)

	res, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)

	def, err := anypb.New(&shv1.Target{
		Run: []string{"echo", "hello"},
	})
	require.NoError(t, err)

	outr, err := hpipe.Reader(ctx, srv.Client(), srv.URL, res.Msg.Path)
	require.NoError(t, err)

	go func() {
		_, err = p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
			Target: &pluginv1.TargetDef{
				Ref: &pluginv1.TargetRef{
					Package: "some/pkg",
					Name:    "target",
					Driver:  "exec",
				},
				Def: def,
			},
			SandboxPath: sandboxPath,
			Pipes:       []string{"", res.Msg.Id, ""},
		}))
		require.NoError(t, err)
	}()

	var stdout bytes.Buffer
	_, err = io.Copy(&stdout, outr)
	require.NoError(t, err)

	assert.Equal(t, "hello\n", stdout.String())
}

func TestPipeStdin(t *testing.T) {
	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(sandboxPath)

	p := New()

	_, rpcHandler := pluginv1connect.NewDriverHandler(p)

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		path, h := p.PipesHandler()
		if strings.HasPrefix(req.URL.Path, path) {
			h.ServeHTTP(w, req)
		} else {
			rpcHandler.ServeHTTP(w, req)
		}
	}))
	defer srv.Close()

	pc := pluginv1connect.NewDriverClient(srv.Client(), srv.URL)

	pipeIn, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)
	pipeOut, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)

	def, err := anypb.New(&shv1.Target{
		Run: []string{"cat"},
	})
	require.NoError(t, err)

	inw, err := hpipe.Writer(ctx, srv.Client(), srv.URL, pipeIn.Msg.Path)
	require.NoError(t, err)

	go func() {
		_, err := io.Copy(inw, strings.NewReader("hello world"))
		if err != nil {
			panic(err)
		}
		err = inw.Close()
		if err != nil {
			panic(err)
		}
	}()

	outr, err := hpipe.Reader(ctx, srv.Client(), srv.URL, pipeOut.Msg.Path)
	require.NoError(t, err)

	go func() {
		_, err = p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
			Target: &pluginv1.TargetDef{
				Ref: &pluginv1.TargetRef{
					Package: "some/pkg",
					Name:    "target",
					Driver:  "exec",
				},
				Def: def,
			},
			SandboxPath: sandboxPath,
			Pipes:       []string{pipeIn.Msg.Id, pipeOut.Msg.Id, ""},
		}))
		require.NoError(t, err)
	}()

	var stdout bytes.Buffer
	_, err = io.Copy(&stdout, outr)
	require.NoError(t, err)

	assert.Equal(t, "hello world", stdout.String())
}

type sleepReader struct {
	d time.Duration
}

func (s sleepReader) Read(p []byte) (n int, err error) {
	time.Sleep(s.d)

	return 0, io.EOF
}

func TestPipeStdinLargeAndSlow(t *testing.T) {
	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(sandboxPath)

	p := New()

	_, rpcHandler := pluginv1connect.NewDriverHandler(p)

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		path, h := p.PipesHandler()
		if strings.HasPrefix(req.URL.Path, path) {
			h.ServeHTTP(w, req)
		} else {
			rpcHandler.ServeHTTP(w, req)
		}
	}))
	defer srv.Close()

	pc := pluginv1connect.NewDriverClient(srv.Client(), srv.URL)

	pipeIn, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)
	pipeOut, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)

	def, err := anypb.New(&shv1.Target{
		Run: []string{"cat"},
	})
	require.NoError(t, err)

	inw, err := hpipe.Writer(ctx, srv.Client(), srv.URL, pipeIn.Msg.Path)
	require.NoError(t, err)

	expected := strings.Repeat("hello world", 10000)

	go func() {
		_, err := io.Copy(inw, io.MultiReader(strings.NewReader(expected), sleepReader{d: time.Second}, strings.NewReader(expected)))
		if err != nil {
			panic(err)
		}
		err = inw.Close()
		if err != nil {
			panic(err)
		}
	}()

	outr, err := hpipe.Reader(ctx, srv.Client(), srv.URL, pipeOut.Msg.Path)
	require.NoError(t, err)

	go func() {
		_, err = p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
			Target: &pluginv1.TargetDef{
				Ref: &pluginv1.TargetRef{
					Package: "some/pkg",
					Name:    "target",
					Driver:  "exec",
				},
				Def: def,
			},
			SandboxPath: sandboxPath,
			Pipes:       []string{pipeIn.Msg.Id, pipeOut.Msg.Id, ""},
		}))
		require.NoError(t, err)
	}()

	var stdout bytes.Buffer
	_, err = io.Copy(&stdout, outr)
	require.NoError(t, err)

	assert.Equal(t, expected+expected, stdout.String())
}

func TestPipe404(t *testing.T) {
	t.Skip() // This is expected to block forever, since there is no functional reader

	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(sandboxPath)

	p := New()

	_, rpcHandler := pluginv1connect.NewDriverHandler(p)

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if strings.HasPrefix(req.URL.Path, PipesHandlerPath) {
			w.WriteHeader(http.StatusNotFound)
			w.Write([]byte("Not found :("))
		} else {
			rpcHandler.ServeHTTP(w, req)
		}
	}))
	defer srv.Close()

	pc := pluginv1connect.NewDriverClient(srv.Client(), srv.URL)

	res, err := pc.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
	require.NoError(t, err)

	def, err := anypb.New(&shv1.Target{
		Run: []string{"echo", "hello"},
	})
	require.NoError(t, err)

	outr, err := hpipe.Reader(ctx, srv.Client(), srv.URL, res.Msg.Path)
	require.NoError(t, err)

	var stdout bytes.Buffer

	var eg errgroup.Group
	eg.Go(func() error {
		_, err = io.Copy(&stdout, outr)

		return err
	})

	_, err = p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
		Target: &pluginv1.TargetDef{
			Ref: &pluginv1.TargetRef{
				Package: "some/pkg",
				Name:    "target",
				Driver:  "exec",
			},
			Def: def,
		},
		SandboxPath: sandboxPath,
		Pipes:       []string{"", res.Msg.Id, ""},
	}))
	require.NoError(t, err)

	err = eg.Wait()
	assert.ErrorContains(t, err, "status: 404 404 Not Found")

	assert.Equal(t, "Not found :(", stdout.String())
}
