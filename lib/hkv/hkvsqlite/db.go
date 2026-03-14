package hkvsqlite

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"iter"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hsync"
	"github.com/hephbuild/heph/lib/hkv"
)

type KV struct {
	path string
	once sync.Once
	rdb  *sql.DB
	wdb  *sql.DB
	err  error

	rpool hsync.Pool[[]byte]

	readerStmt *sql.Stmt
	existsStmt *sql.Stmt
	upsertStmt *sql.Stmt
	deleteStmt *sql.Stmt

	insertMetaStmt *sql.Stmt
	deleteMetaStmt *sql.Stmt
	getMetaStmt    *sql.Stmt

	listStmt *sql.Stmt
}

var _ hkv.KV[[]byte] = (*KV)(nil)
var _ hkv.IO = (*KV)(nil)
var _ hkv.Lister = (*KV)(nil)

const sqlCacheDataSize = 100_000

func New(path string) *KV {
	return &KV{
		path: path,
		rpool: hsync.Pool[[]byte]{New: func() []byte {
			return make([]byte, sqlCacheDataSize)
		}},
	}
}

func (c *KV) open(ctx context.Context) (*sql.DB, *sql.DB, error) {
	c.once.Do(func() {
		err := c.openInner(ctx)
		if err != nil {
			c.err = fmt.Errorf("sqlcache open: %w", err)
			_ = c.Close()
			return
		}
	})
	return c.rdb, c.wdb, c.err
}

func (c *KV) migrate(ctx context.Context) error {
	_, err := c.wdb.ExecContext(ctx, `
		CREATE TABLE IF NOT EXISTS kv (
			key      TEXT NOT NULL,
			data     BLOB,
			PRIMARY KEY (key)
		);

		CREATE INDEX IF NOT EXISTS kv_key ON kv (key);

		CREATE TABLE IF NOT EXISTS kv_meta (
			key  TEXT NOT NULL,
			mkey TEXT NOT NULL,
			mval TEXT NOT NULL,
			PRIMARY KEY (key, mkey),
			FOREIGN KEY (key) REFERENCES kv(key) ON DELETE CASCADE
		);

		CREATE INDEX IF NOT EXISTS kv_meta_mkey_mval ON kv_meta (mkey, mval);
	`)

	return err
}

func (c *KV) Migrate(ctx context.Context) error {
	_, _, err := c.open(ctx)
	return err
}

func (c *KV) openInner(ctx context.Context) error {
	if err := os.MkdirAll(filepath.Dir(c.path), os.ModePerm); err != nil {
		return fmt.Errorf("mkdir: %w", err)
	}

	connConfig := ConnConfig{
		CacheSize: 64000,
		PageSize:  8192,
		MmapSize:  268435456,
	}

	wdb := sql.OpenDB(newConnector(c.path, connConfig))
	// One writer at a time — prevents SQLITE_BUSY.
	wdb.SetMaxOpenConns(1)
	wdb.SetMaxIdleConns(1)

	rdb := sql.OpenDB(newConnector(c.path, connConfig))
	// No cap — WAL lets concurrent readers run in parallel.
	rdb.SetMaxIdleConns(100)
	rdb.SetMaxOpenConns(100)

	c.rdb = rdb
	c.wdb = wdb

	if err := c.migrate(ctx); err != nil {
		return err
	}

	var err error
	c.readerStmt, err = rdb.PrepareContext(ctx, `
			SELECT data
			FROM kv
			WHERE key = ?
			LIMIT 1
		`)
	if err != nil {
		return fmt.Errorf("prepare reader: %w", err)
	}

	c.existsStmt, err = rdb.PrepareContext(ctx, `SELECT 1 FROM kv WHERE key = ? LIMIT 1`)
	if err != nil {
		return fmt.Errorf("prepare exists: %w", err)
	}

	c.upsertStmt, err = wdb.PrepareContext(ctx, `
		INSERT INTO kv (key, data)
		VALUES (?, ?)
		ON CONFLICT(key) DO UPDATE SET
			data     = excluded.data
		`)
	if err != nil {
		return fmt.Errorf("prepare upsert: %w", err)
	}

	c.deleteStmt, err = wdb.PrepareContext(ctx, `DELETE FROM kv WHERE key = ?`)
	if err != nil {
		return fmt.Errorf("prepare delete: %w", err)
	}

	c.insertMetaStmt, err = wdb.PrepareContext(ctx, `INSERT INTO kv_meta (key, mkey, mval) VALUES (?, ?, ?)`)
	if err != nil {
		return fmt.Errorf("prepare insert meta: %w", err)
	}

	c.deleteMetaStmt, err = wdb.PrepareContext(ctx, `DELETE FROM kv_meta WHERE key = ?`)
	if err != nil {
		return fmt.Errorf("prepare delete meta: %w", err)
	}

	c.getMetaStmt, err = rdb.PrepareContext(ctx, `SELECT mkey, mval FROM kv_meta WHERE key = ?`)
	if err != nil {
		return fmt.Errorf("prepare get meta: %w", err)
	}

	c.listStmt, err = rdb.PrepareContext(ctx, `
		SELECT k.key
		FROM kv k
		WHERE (SELECT COUNT(*) FROM json_each(?)) = (
			SELECT COUNT(*)
			FROM kv_meta km
			WHERE km.key = k.key
			AND (km.mkey, km.mval) IN (SELECT key, value FROM json_each(?))
		)
	`)
	if err != nil {
		return fmt.Errorf("prepare list with metadata: %w", err)
	}

	return nil
}

func (c *KV) Get(ctx context.Context, key string) ([]byte, map[string]string, bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, nil, false, err
	}

	r, ok, err := c.Reader(ctx, key)
	if err != nil {
		return nil, nil, false, err
	}

	if !ok {
		return nil, nil, false, nil
	}
	defer r.Close()

	meta, _, err := c.GetMeta(ctx, key)
	if err != nil {
		return nil, nil, false, err
	}

	b, err := io.ReadAll(r)
	if err != nil {
		return nil, nil, false, err
	}

	return b, meta, true, nil
}

func (c *KV) GetMeta(ctx context.Context, key string) (map[string]string, bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, false, err
	}

	rows, err := c.getMetaStmt.QueryContext(ctx, key)
	if err != nil {
		return nil, false, err
	}
	defer rows.Close()

	meta := make(map[string]string)
	for rows.Next() {
		var k, v string
		if err := rows.Scan(&k, &v); err != nil {
			return nil, false, err
		}
		meta[k] = v
	}

	if err := rows.Err(); err != nil {
		return nil, false, err
	}

	if len(meta) == 0 {
		exists, err := c.Exists(ctx, key)
		if err != nil {
			return nil, false, err
		}
		if !exists {
			return nil, false, nil
		}
	}

	return meta, true, nil
}

func (c *KV) Exists(ctx context.Context, key string) (bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return false, err
	}

	var exists bool
	err = c.existsStmt.QueryRowContext(ctx, key).Scan(&exists)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, fmt.Errorf("exists: %w", err)
	}

	return exists, nil
}

func (c *KV) Set(ctx context.Context, key string, value []byte, metadata map[string]string, ttl time.Duration) error {
	if ttl > 0 {
		return errors.New("unsupported")
	}

	w, err := c.Writer(ctx, key, metadata, ttl)
	if err != nil {
		return err
	}
	defer w.Close()

	_, err = w.Write(value)
	if err != nil {
		return err
	}

	return w.Close()
}

func (c *KV) Delete(ctx context.Context, key string) error {
	_, _, err := c.open(ctx)
	if err != nil {
		return err
	}

	_, err = c.deleteStmt.ExecContext(
		ctx,
		key,
	)
	if err != nil {
		return fmt.Errorf("delete: %w", err)
	}

	return nil
}

func (c *KV) ListKeys(ctx context.Context, query map[string]string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		_, _, err := c.open(ctx)
		if err != nil {
			yield("", err)
			return
		}

		// Use a single prepared statement with JSON filters for better performance and simplicity
		queryJSON := []byte("{}")
		if len(query) > 0 {
			queryJSON, err = json.Marshal(query)
			if err != nil {
				yield("", err)
				return
			}
		}

		rows, err := c.listStmt.QueryContext(ctx, queryJSON, queryJSON)
		if err != nil {
			yield("", err)
			return
		}
		defer rows.Close()

		for rows.Next() {
			var key string
			if err := rows.Scan(&key); err != nil {
				if !yield("", err) {
					return
				}

				continue
			}
			if !yield(key, nil) {
				return
			}
		}

		if err := rows.Err(); err != nil {
			yield("", err)
		}
	}
}

func (c *KV) Close() error {
	var errs []error
	if c.readerStmt != nil {
		errs = append(errs, c.readerStmt.Close())
	}
	if c.existsStmt != nil {
		errs = append(errs, c.existsStmt.Close())
	}
	if c.deleteStmt != nil {
		errs = append(errs, c.deleteStmt.Close())
	}
	if c.upsertStmt != nil {
		errs = append(errs, c.upsertStmt.Close())
	}
	if c.insertMetaStmt != nil {
		errs = append(errs, c.insertMetaStmt.Close())
	}
	if c.deleteMetaStmt != nil {
		errs = append(errs, c.deleteMetaStmt.Close())
	}
	if c.listStmt != nil {
		errs = append(errs, c.listStmt.Close())
	}
	if c.rdb != nil {
		errs = append(errs, c.rdb.Close())
	}
	if c.wdb != nil {
		errs = append(errs, c.wdb.Close())
	}

	return errors.Join(errs...)
}
