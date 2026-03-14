package hkvsqlite

import (
	"bytes"
	"context"
	"database/sql"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/hio"
)

func (c *KV) Reader(ctx context.Context, key string) (io.ReadCloser, bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, false, err
	}

	rows, err := c.readerStmt.QueryContext(
		ctx,
		key,
	)
	if err != nil {
		return nil, false, fmt.Errorf("reader query: %w", err)
	}
	defer rows.Close()

	if !rows.Next() {
		if err := rows.Err(); err != nil {
			return nil, false, fmt.Errorf("reader next: %w", err)
		}
		return nil, false, nil
	}

	var raw sql.RawBytes
	err = rows.Scan(&raw)
	if err != nil {
		return nil, false, fmt.Errorf("reader scan: %w", err)
	}

	if len(raw) > sqlCacheDataSize {
		data := bytes.Clone(raw)

		return io.NopCloser(bytes.NewReader(data)), true, nil
	}

	buf := c.rpool.Get()
	onClose := func() error {
		c.rpool.Put(buf)

		return nil
	}
	n := copy(buf, raw)

	data := buf[:n]

	return hio.NewReadCloserFunc(bytes.NewReader(data), onClose), true, nil
}
