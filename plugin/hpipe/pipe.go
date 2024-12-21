package hpipe

import (
	"context"
	"io"
	"net/http"
)

type ReaderCloser interface {
	io.Reader
	io.Closer
}

func Reader(ctx context.Context, client *http.Client, baseUrl, path string) (ReaderCloser, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, baseUrl+path, nil)
	if err != nil {
		return nil, err
	}

	r, w := io.Pipe()

	go func() {
		res, err := client.Do(req)
		if err != nil {
			_ = w.CloseWithError(err)
			return
		}

		_, err = io.Copy(w, res.Body)
		_ = w.CloseWithError(err)
	}()

	return r, nil
}

type WriterCloser interface {
	io.Writer
	io.Closer
}

func Writer(ctx context.Context, client *http.Client, baseUrl, path string) (WriterCloser, error) {
	r, w := io.Pipe()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseUrl+path, r)
	if err != nil {
		return nil, err
	}

	go func() {
		res, err := client.Do(req)
		if err != nil {
			_ = w.CloseWithError(err)
			return
		}
		defer res.Body.Close()
	}()

	return w, nil
}
