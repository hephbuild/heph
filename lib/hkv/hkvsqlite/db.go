package hkvsqlite

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"os"
	"path/filepath"
	"runtime"
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

	insertMetaStmt  *sql.Stmt
	deleteMetaStmt  *sql.Stmt
	getMetaStmt     *sql.Stmt
	getCombinedStmt *sql.Stmt

	listStmt *sql.Stmt
	gcStmt   *sql.Stmt
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
			key       TEXT NOT NULL,
			data      BLOB,
			expire_at INTEGER,
			PRIMARY KEY (key)
		);

		CREATE INDEX IF NOT EXISTS kv_key ON kv (key);
		CREATE INDEX IF NOT EXISTS kv_expire_at ON kv (expire_at);

		DROP TABLE IF EXISTS kv_meta;
	`)
	if err != nil {
		return err
	}

	_, _ = c.wdb.ExecContext(ctx, `ALTER TABLE kv ADD COLUMN meta TEXT;`)

	return nil
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
	wdb.SetConnMaxLifetime(0)
	wdb.SetConnMaxIdleTime(0)

	rdb := sql.OpenDB(newConnector(c.path, connConfig))
	// No cap — WAL lets concurrent readers run in parallel.
	rdb.SetMaxIdleConns(runtime.GOMAXPROCS(0) + 1)
	rdb.SetMaxOpenConns(runtime.GOMAXPROCS(0) + 1)
	rdb.SetConnMaxLifetime(0)
	rdb.SetConnMaxIdleTime(0)

	c.rdb = rdb
	c.wdb = wdb

	if err := c.migrate(ctx); err != nil {
		return err
	}

	var err error
	c.readerStmt, err = rdb.PrepareContext(ctx, `
			SELECT data
			FROM kv
			WHERE key = ? AND (expire_at IS NULL OR expire_at > ?)
			LIMIT 1
		`)
	if err != nil {
		return fmt.Errorf("prepare reader: %w", err)
	}

	c.existsStmt, err = rdb.PrepareContext(ctx, `SELECT 1 FROM kv WHERE key = ? AND (expire_at IS NULL OR expire_at > ?) LIMIT 1`)
	if err != nil {
		return fmt.Errorf("prepare exists: %w", err)
	}

	c.upsertStmt, err = wdb.PrepareContext(ctx, `
		INSERT INTO kv (key, data, expire_at, meta)
		VALUES (?, ?, ?, ?)
		ON CONFLICT(key) DO UPDATE SET
			data      = excluded.data,
			expire_at = excluded.expire_at,
			meta      = excluded.meta
		`)
	if err != nil {
		return fmt.Errorf("prepare upsert: %w", err)
	}

	c.deleteStmt, err = wdb.PrepareContext(ctx, `DELETE FROM kv WHERE key = ?`)
	if err != nil {
		return fmt.Errorf("prepare delete: %w", err)
	}

	c.getMetaStmt, err = rdb.PrepareContext(ctx, `SELECT meta FROM kv WHERE key = ? AND (expire_at IS NULL OR expire_at > ?) LIMIT 1`)
	if err != nil {
		return fmt.Errorf("prepare get meta: %w", err)
	}

	c.getCombinedStmt, err = rdb.PrepareContext(ctx, `SELECT data, meta FROM kv WHERE key = ? AND (expire_at IS NULL OR expire_at > ?) LIMIT 1`)
	if err != nil {
		return fmt.Errorf("prepare get combined: %w", err)
	}

	c.listStmt, err = rdb.PrepareContext(ctx, `
		SELECT key
		FROM kv
		WHERE (expire_at IS NULL OR expire_at > ?)
		AND (
			? = '{}' OR
			(SELECT COUNT(*) FROM json_each(?)) = (
				SELECT COUNT(*)
				FROM json_each(kv.meta) km
				WHERE (km.key, km.value) IN (SELECT key, value FROM json_each(?))
			)
		)
	`)
	if err != nil {
		return fmt.Errorf("prepare list with metadata: %w", err)
	}

	c.gcStmt, err = wdb.PrepareContext(ctx, `DELETE FROM kv WHERE expire_at IS NOT NULL AND expire_at <= ?`)
	if err != nil {
		return fmt.Errorf("prepare gc: %w", err)
	}

	return nil
}

func (c *KV) Get(ctx context.Context, key string) ([]byte, map[string]string, bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, nil, false, err
	}

	rows, err := c.getCombinedStmt.QueryContext(ctx, key, time.Now().Unix())
	if err != nil {
		return nil, nil, false, err
	}
	defer rows.Close()

	if !rows.Next() {
		if err := rows.Err(); err != nil {
			return nil, nil, false, err
		}
		return nil, nil, false, nil
	}

	var raw sql.RawBytes
	var metaJSON sql.NullString
	if err := rows.Scan(&raw, &metaJSON); err != nil {
		return nil, nil, false, err
	}

	data := make([]byte, len(raw))
	copy(data, raw)

	var meta map[string]string
	if metaJSON.Valid {
		if err := json.Unmarshal([]byte(metaJSON.String), &meta); err != nil {
			return nil, nil, false, err
		}
	} else {
		meta = make(map[string]string)
	}

	return data, meta, true, nil
}

func (c *KV) GetMeta(ctx context.Context, key string) (map[string]string, bool, error) {
	_, _, err := c.open(ctx)
	if err != nil {
		return nil, false, err
	}

	var metaJSON sql.NullString
	err = c.getMetaStmt.QueryRowContext(ctx, key, time.Now().Unix()).Scan(&metaJSON)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, false, nil
		}
		return nil, false, err
	}

	var meta map[string]string
	if metaJSON.Valid {
		if err := json.Unmarshal([]byte(metaJSON.String), &meta); err != nil {
			return nil, false, err
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
	err = c.existsStmt.QueryRowContext(ctx, key, time.Now().Unix()).Scan(&exists)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, fmt.Errorf("exists: %w", err)
	}

	return exists, nil
}

func (c *KV) Set(ctx context.Context, key string, value []byte, metadata map[string]string, ttl time.Duration) error {
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

		queryJSONSTR := string(queryJSON)
		rows, err := c.listStmt.QueryContext(ctx, time.Now().Unix(), queryJSONSTR, queryJSONSTR, queryJSONSTR)
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

func (c *KV) GC(ctx context.Context) error {
	_, wdb, err := c.open(ctx)
	if err != nil {
		return err
	}

	_, err = c.gcStmt.ExecContext(ctx, time.Now().Unix())
	if err != nil {
		return err
	}

	_, err = wdb.ExecContext(ctx, "PRAGMA incremental_vacuum(500);")

	return err
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
	if c.getCombinedStmt != nil {
		errs = append(errs, c.getCombinedStmt.Close())
	}
	if c.listStmt != nil {
		errs = append(errs, c.listStmt.Close())
	}
	if c.gcStmt != nil {
		errs = append(errs, c.gcStmt.Close())
	}
	if c.rdb != nil {
		errs = append(errs, c.rdb.Close())
	}
	if c.wdb != nil {
		errs = append(errs, c.wdb.Close())
	}

	return errors.Join(errs...)
}
