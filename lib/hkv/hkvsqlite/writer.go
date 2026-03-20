package hkvsqlite

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"sync"
	"time"
)

type kvWriter struct {
	pw        *io.PipeWriter
	done      <-chan error
	closeOnce sync.Once
	closeErr  error
}

func (w *kvWriter) Write(p []byte) (int, error) {
	return w.pw.Write(p)
}

func (w *kvWriter) Close() error {
	w.closeOnce.Do(func() {
		// Close the write end; this unblocks the goroutine's reader.
		if err := w.pw.Close(); err != nil {
			w.closeErr = err
			return
		}
		// Wait for the goroutine to finish and return any write error.
		w.closeErr = <-w.done
	})
	return w.closeErr
}

func (c *KV) writeEntry(ctx context.Context, key string, metadata map[string]string, ttl time.Duration, data io.Reader) error {
	payload, err := io.ReadAll(data)
	if err != nil {
		return fmt.Errorf("writeEntry read: %w", err)
	}

	_, wdb, err := c.open(ctx)
	if err != nil {
		return err
	}

	tx, err := wdb.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("writeEntry begin: %w", err)
	}
	defer tx.Rollback()

	var expireAt *int64
	if ttl != 0 {
		t := time.Now().Add(ttl).Unix()
		expireAt = &t
	}

	var metaJSON []byte
	if len(metadata) > 0 {
		metaJSON, err = json.Marshal(metadata)
		if err != nil {
			return fmt.Errorf("writeEntry marshal meta: %w", err)
		}
	}

	// Single UPSERT — write lock held for exactly one statement.
	_, err = tx.StmtContext(ctx, c.upsertStmt).ExecContext(
		ctx,
		key, payload, expireAt, metaJSON,
	)
	if err != nil {
		return fmt.Errorf("writeEntry upsert: %w", err)
	}

	return tx.Commit()
}

func (c *KV) Writer(ctx context.Context, key string, metadata map[string]string, ttl time.Duration) (io.WriteCloser, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, err
	}

	pr, pw := io.Pipe()
	done := make(chan error, 1)

	go func() {
		defer pr.Close()
		err := c.writeEntry(ctx, key, metadata, ttl, pr)
		if err != nil {
			wrappedErr := fmt.Errorf("writer write: %q %w", key, err)
			_ = pr.CloseWithError(wrappedErr)
			done <- wrappedErr
		} else {
			done <- nil
		}
		close(done)
	}()

	return &kvWriter{pw: pw, done: done}, nil
}
