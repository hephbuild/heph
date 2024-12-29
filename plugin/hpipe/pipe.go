package hpipe

import (
	"context"
	"fmt"
	"io"
	"net/http"
)

type ReaderCloser interface {
	io.Reader
	io.Closer
}

func Reader(ctx context.Context, client *http.Client, baseURL, path string) (ReaderCloser, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, baseURL+path, nil)
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

func Writer(ctx context.Context, client *http.Client, baseURL, path string) (WriterCloser, error) {
	r, w := io.Pipe()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+path, r)
	if err != nil {
		return nil, err
	}
	req.ContentLength = -1

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
