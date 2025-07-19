package hpipe

import (
	"context"
	"fmt"
	"io"
	"net/http"

	"go.opentelemetry.io/otel"
)

type ReaderCloser interface {
	io.Reader
	io.Closer
}

func Reader(ctx context.Context, client *http.Client, baseURL, path string) (ReaderCloser, error) {
	ctx, span := tracer.Start(ctx, "Reader")

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, baseURL+path, nil)
	if err != nil {
		span.End()

		return nil, err
	}

	r, w := io.Pipe()

	go func() {
		defer span.End()

		res, err := client.Do(req)
		if err != nil {
			_ = w.CloseWithError(err)
			return
		}
		defer res.Body.Close()

		_, err = io.Copy(w, res.Body)
		if err == nil && res.StatusCode != http.StatusOK {
			err = fmt.Errorf("status: %v %v", res.StatusCode, res.Status)
		}
		_ = w.CloseWithError(err)
	}()

	return r, nil
}

type WriterCloser interface {
	io.Writer
	io.Closer
}

var tracer = otel.Tracer("heph/hpipe")

func Writer(ctx context.Context, client *http.Client, baseURL, path string) (WriterCloser, error) {
	ctx, span := tracer.Start(ctx, "Writer")

	r, w := io.Pipe()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+path, r)
	if err != nil {
		span.End()

		return nil, err
	}
	req.ContentLength = -1

	go func() {
		defer span.End()

		res, err := client.Do(req)
		if err != nil {
			_ = w.CloseWithError(err)
			return
		}
		defer res.Body.Close()
	}()

	return w, nil
}
