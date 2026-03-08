package engine

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"iter"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/hsync"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"modernc.org/sqlite"
)

// SQLCacheDB holds the path and lazily opens both connection pools on first use.
// Call Close only if the DB was actually used; it is safe to call regardless.
type SQLCacheDB struct {
	path       string
	once       sync.Once
	rdb        *sql.DB
	wdb        *sql.DB
	readerStmt *sql.Stmt
	existsStmt *sql.Stmt
	err        error
}

func (s *SQLCacheDB) pools() (*sql.DB, *sql.DB, error) {
	ctx := context.Background()

	s.once.Do(func() {
		sqlite.RegisterConnectionHook(func(conn sqlite.ExecQuerierContext, _ string) error {
			_, err := conn.ExecContext(ctx, `
				PRAGMA journal_mode = WAL;
				PRAGMA busy_timeout = 10000;
				PRAGMA synchronous = NORMAL;
				PRAGMA foreign_keys = ON;
				PRAGMA cache_size = -64000;
				PRAGMA page_size = 8192;
				PRAGMA mmap_size = 268435456;
				PRAGMA temp_store = MEMORY;
			`, nil)
			return err
		})

		if err := os.MkdirAll(filepath.Dir(s.path), os.ModePerm); err != nil {
			s.err = fmt.Errorf("OpenSQLCacheDB mkdir: %w", err)
			return
		}

		wdb, err := openSQLiteDB(s.path)
		if err != nil {
			s.err = fmt.Errorf("OpenSQLCacheDB open wdb: %w", err)
			return
		}
		// One writer at a time — prevents SQLITE_BUSY.
		wdb.SetMaxOpenConns(1)

		if err := initSQLCacheDB(ctx, wdb); err != nil {
			_ = wdb.Close()
			s.err = fmt.Errorf("OpenSQLCacheDB init: %w", err)
			return
		}

		rdb, err := openSQLiteDB(s.path)
		if err != nil {
			_ = wdb.Close()
			s.err = fmt.Errorf("OpenSQLCacheDB open rdb: %w", err)
			return
		}
		// No cap — WAL lets concurrent readers run in parallel.
		rdb.SetMaxIdleConns(100)
		rdb.SetMaxOpenConns(100)

		readerStmt, err := rdb.PrepareContext(ctx, `
			SELECT data
			FROM cache_blobs
			WHERE target_addr = ? AND hashin = ? AND artifact_name = ?
		`)
		if err != nil {
			_ = rdb.Close()
			_ = wdb.Close()
			s.err = fmt.Errorf("OpenSQLCacheDB prepare reader: %w", err)
			return
		}

		existsStmt, err := rdb.PrepareContext(ctx, `SELECT 1 FROM cache_blobs WHERE target_addr = ? AND hashin = ? AND artifact_name = ? LIMIT 1`)
		if err != nil {
			_ = readerStmt.Close() //nolint:sqlclosecheck
			_ = rdb.Close()
			_ = wdb.Close()
			s.err = fmt.Errorf("OpenSQLCacheDB prepare exists: %w", err)
			return
		}

		s.rdb = rdb
		s.wdb = wdb
		s.readerStmt = readerStmt
		s.existsStmt = existsStmt
	})
	return s.rdb, s.wdb, s.err
}

func (s *SQLCacheDB) Close() error {
	// If pools() was never called, once.Do has never run and rdb/wdb are nil.
	if s.rdb == nil {
		return nil
	}
	errR := s.rdb.Close()
	errW := s.wdb.Close()
	errS := s.readerStmt.Close()
	errE := s.existsStmt.Close()
	if errR != nil {
		return errR
	}
	if errS != nil {
		return errS
	}
	if errE != nil {
		return errE
	}
	return errW
}

type SQLCache struct {
	db *SQLCacheDB

	rpool hsync.Pool[[]byte]
}

func (c *SQLCache) rwdb(ctx context.Context) (*sql.DB, *sql.DB, error) {
	rdb, wdb, err := c.db.pools()
	if err != nil {
		return nil, nil, fmt.Errorf("sqlcache open db: %w", err)
	}

	return rdb, wdb, nil
}

func (c *SQLCache) Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error) {
	_, _, err := c.rwdb(ctx)
	if err != nil {
		return false, err
	}

	targetAddr := c.targetKey(ref)

	var exists bool
	err = c.db.existsStmt.QueryRowContext(
		ctx,
		targetAddr, hashin, name,
	).Scan(&exists)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, fmt.Errorf("exists: %w", err)
	}

	return exists, nil
}

func (c *SQLCache) Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error {
	_, wdb, err := c.rwdb(ctx)
	if err != nil {
		return err
	}

	targetAddr := c.targetKey(ref)

	if name == "" {
		_, err = wdb.ExecContext(
			ctx,
			`DELETE FROM cache_blobs WHERE target_addr = ? AND hashin = ?`,
			targetAddr, hashin,
		)
	} else {
		_, err = wdb.ExecContext(
			ctx,
			`DELETE FROM cache_blobs WHERE target_addr = ? AND hashin = ? AND artifact_name = ?`,
			targetAddr, hashin, name,
		)
	}
	if err != nil {
		return fmt.Errorf("delete: %w", err)
	}

	return nil
}

var _ LocalCache = (*SQLCache)(nil)

func initSQLCacheDB(ctx context.Context, db *sql.DB) error {
	_, err := db.ExecContext(ctx, `
		CREATE TABLE IF NOT EXISTS cache_blobs (
			target_addr   TEXT    NOT NULL,
			hashin        TEXT    NOT NULL,
			artifact_name TEXT    NOT NULL,
			data          BLOB,
			created_at    INTEGER NOT NULL,
			PRIMARY KEY (target_addr, hashin, artifact_name)
		);

		CREATE INDEX IF NOT EXISTS cache_blobs_target_hashin
			ON cache_blobs (target_addr, hashin);
	`)

	return err
}

// openSQLiteDB opens a connection pool to a SQLite file with common pragmas.
func openSQLiteDB(path string) (*sql.DB, error) {
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, err
	}
	return db, nil
}

// OpenSQLCacheDB returns a SQLCacheDB that opens its connection pools lazily
// on first use. No file I/O happens until the first cache operation.
func OpenSQLCacheDB(path string) (*SQLCacheDB, error) {
	return &SQLCacheDB{path: path}, nil
}

func NewSQLCache(db *SQLCacheDB) *SQLCache {
	return &SQLCache{
		db: db,
		rpool: hsync.Pool[[]byte]{New: func() []byte {
			return make([]byte, 100_000)
		}},
	}
}

// targetAddr computes a stable filesystem-safe address for a TargetRef.
// When the ref has no args, this is just "__<name>". When args are present,
// a hash of the full ref is appended to disambiguate.
func (c *SQLCache) targetAddr(ref *pluginv1.TargetRef) string {
	if len(ref.GetArgs()) == 0 {
		return ref.GetName()
	}

	h := xxh3.New()
	hashpb.Hash(h, ref, tref.OmitHashPb)

	return ref.GetName() + "@" + hex.EncodeToString(h.Sum(nil))
}

// targetKey returns the compound target address used as target_addr in the DB.
func (c *SQLCache) targetKey(ref *pluginv1.TargetRef) string {
	return ref.GetPackage() + ":" + c.targetAddr(ref)
}

func (c *SQLCache) Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error) {
	_, _, err := c.rwdb(ctx)
	if err != nil {
		return nil, err
	}

	targetAddr := c.targetKey(ref)

	rows, err := c.db.readerStmt.QueryContext(
		ctx,
		targetAddr, hashin, name,
	)
	if err != nil {
		return nil, fmt.Errorf("reader query: %w", err)
	}
	defer rows.Close()

	if !rows.Next() {
		if err := rows.Err(); err != nil {
			return nil, fmt.Errorf("reader next: %w", err)
		}
		return nil, ErrLocalCacheNotFound
	}

	var raw sql.RawBytes
	err = rows.Scan(&raw)
	if err != nil {
		return nil, fmt.Errorf("reader scan: %w", err)
	}

	buf := c.rpool.Get()
	buf = append(buf[:0], raw...)

	return hio.NewReadCloserFunc(bytes.NewReader(buf), func() error {
		c.rpool.Put(buf)

		return nil
	}), nil
}

func (c *SQLCache) writeEntry(ctx context.Context, targetAddr, hashin, name string, data io.Reader) error {
	_, wdb, err := c.rwdb(ctx)
	if err != nil {
		return err
	}

	payload, err := io.ReadAll(data)
	if err != nil {
		return fmt.Errorf("writeEntry read: %w", err)
	}

	// Single UPSERT — write lock held for exactly one statement.
	_, err = wdb.ExecContext(
		ctx,
		`
		INSERT INTO cache_blobs (target_addr, hashin, artifact_name, data, created_at)
		VALUES (?, ?, ?, ?, ?)
		ON CONFLICT(target_addr, hashin, artifact_name) DO UPDATE SET
			data       = excluded.data,
			created_at = excluded.created_at
		`,
		targetAddr, hashin, name, payload, time.Now().UnixNano(),
	)
	if err != nil {
		return fmt.Errorf("writeEntry upsert: %w", err)
	}

	return nil
}

type sqlCacheWriter struct {
	pw        *io.PipeWriter
	done      <-chan error
	closeOnce sync.Once
	closeErr  error
}

func (w *sqlCacheWriter) Write(p []byte) (int, error) {
	return w.pw.Write(p)
}

func (w *sqlCacheWriter) Close() error {
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

func (c *SQLCache) Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error) {
	targetAddr := c.targetKey(ref)

	pr, pw := io.Pipe()
	done := make(chan error, 1)

	go func() {
		defer pr.Close()
		err := c.writeEntry(ctx, targetAddr, hashin, name, pr)
		if err != nil {
			wrappedErr := fmt.Errorf("writer write: %q %q %q %w", tref.Format(ref), hashin, name, err)
			_ = pr.CloseWithError(wrappedErr)
			done <- wrappedErr
		} else {
			done <- nil
		}
		close(done)
	}()

	return &sqlCacheWriter{pw: pw, done: done}, nil
}

func (c *SQLCache) ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		rdb, _, err := c.rwdb(ctx)
		if err != nil {
			yield("", err)
			return
		}

		targetAddr := c.targetKey(ref)

		rows, err := rdb.QueryContext(ctx,
			"SELECT artifact_name FROM cache_blobs WHERE target_addr = ? AND hashin = ?",
			targetAddr, hashin,
		)
		if err != nil {
			yield("", err)
			return
		}
		defer rows.Close()

		for rows.Next() {
			var artifactName string
			if err := rows.Scan(&artifactName); err != nil {
				if !yield("", err) {
					return
				}
				continue
			}
			if !yield(artifactName, nil) {
				return
			}
		}
		if err := rows.Err(); err != nil {
			yield("", err)
		}
	}
}

func (c *SQLCache) ListVersions(ctx context.Context, ref *pluginv1.TargetRef) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		rdb, _, err := c.rwdb(ctx)
		if err != nil {
			yield("", err)
			return
		}

		targetAddr := c.targetKey(ref)

		rows, err := rdb.QueryContext(ctx,
			"SELECT DISTINCT hashin FROM cache_blobs WHERE target_addr = ?",
			targetAddr,
		)
		if err != nil {
			yield("", err)
			return
		}
		defer rows.Close()

		for rows.Next() {
			var hashin string
			if err := rows.Scan(&hashin); err != nil {
				if !yield("", err) {
					return
				}
				continue
			}
			if !yield(hashin, nil) {
				return
			}
		}
		if err := rows.Err(); err != nil {
			yield("", err)
		}
	}
}
