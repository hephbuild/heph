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
	"time"

	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/hsync"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	_ "modernc.org/sqlite"
)

// SQLCacheDB holds the two connection pools used by SQLCache.
// Use OpenSQLCacheDB to open it and Close to shut down both pools.
type SQLCacheDB struct {
	// rdb is the read pool: uncapped, WAL allows concurrent readers.
	rdb *sql.DB
	// wdb is the write pool: MaxOpenConns(1) serialises writers.
	wdb *sql.DB
}

func (s *SQLCacheDB) Close() error {
	errR := s.rdb.Close()
	errW := s.wdb.Close()
	if errR != nil {
		return errR
	}
	return errW
}

type SQLCache struct {
	// rdb is used for all read operations. WAL mode allows many concurrent
	// readers, so this pool is uncapped.
	rdb *sql.DB
	// wdb is used for all write operations. SetMaxOpenConns(1) serialises
	// writers at the Go level, preventing SQLITE_BUSY races.
	wdb *sql.DB

	pool hsync.Pool[[]byte]
}

func (c *SQLCache) Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error) {
	targetAddr := c.targetKey(ref)

	var count int
	err := c.rdb.QueryRowContext(
		ctx,
		`SELECT COUNT(*) FROM cache_blobs WHERE target_addr = ? AND hashin = ? AND artifact_name = ? LIMIT 1`,
		targetAddr, hashin, name,
	).Scan(&count)
	if err != nil {
		return false, fmt.Errorf("exists: %w", err)
	}

	return count > 0, nil
}

func (c *SQLCache) Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error {
	targetAddr := c.targetKey(ref)

	var err error
	if name == "" {
		_, err = c.wdb.ExecContext(
			ctx,
			`DELETE FROM cache_blobs WHERE target_addr = ? AND hashin = ?`,
			targetAddr, hashin,
		)
	} else {
		_, err = c.wdb.ExecContext(
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

func migrateSQLCacheDB(db *sql.DB) error {
	_, err := db.Exec(`
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
	dsn := path +
		"?_pragma=journal_mode(WAL)" +
		"&_pragma=busy_timeout(10000)" +
		"&_pragma=synchronous(NORMAL)" +
		"&_pragma=foreign_keys(ON)"

	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	return db, nil
}

// OpenSQLCacheDB opens a SQLCacheDB with two connection pools to the same SQLite file:
//   - wdb: a single-connection write pool (serialises writers, no SQLITE_BUSY)
//   - rdb: an uncapped read pool (WAL allows fully concurrent readers)
func OpenSQLCacheDB(path string) (*SQLCacheDB, error) {
	if err := os.MkdirAll(filepath.Dir(path), os.ModePerm); err != nil {
		return nil, fmt.Errorf("OpenSQLCacheDB mkdir: %w", err)
	}

	wdb, err := openSQLiteDB(path)
	if err != nil {
		return nil, fmt.Errorf("OpenSQLCacheDB open wdb: %w", err)
	}
	// One writer at a time — prevents SQLITE_BUSY without busy-retry overhead.
	wdb.SetMaxOpenConns(1)

	if err := migrateSQLCacheDB(wdb); err != nil {
		_ = wdb.Close()
		return nil, fmt.Errorf("OpenSQLCacheDB migrate: %w", err)
	}

	rdb, err := openSQLiteDB(path)
	if err != nil {
		_ = wdb.Close()
		return nil, fmt.Errorf("OpenSQLCacheDB open rdb: %w", err)
	}
	// No cap — WAL lets concurrent readers run in parallel.

	return &SQLCacheDB{rdb: rdb, wdb: wdb}, nil
}

func NewSQLCache(db *SQLCacheDB) *SQLCache {
	return &SQLCache{
		rdb: db.rdb,
		wdb: db.wdb,
		pool: hsync.Pool[[]byte]{New: func() []byte {
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
	targetAddr := c.targetKey(ref)

	var dataBytes sql.NullString

	err := c.rdb.QueryRowContext(
		ctx,
		`
		SELECT data
		FROM cache_blobs
		WHERE target_addr = ? AND hashin = ? AND artifact_name = ?
		`,
		targetAddr, hashin, name,
	).Scan(&dataBytes)

	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, LocalCacheNotFoundError
		}

		return nil, fmt.Errorf("reader scan: %w", err)
	}

	if !dataBytes.Valid {
		return io.NopCloser(bytes.NewReader(nil)), nil
	}

	scanned := []byte(dataBytes.String)
	return io.NopCloser(bytes.NewReader(scanned)), nil
}

func (c *SQLCache) writeEntry(ctx context.Context, targetAddr, hashin, name string, data io.Reader) error {
	_, err := c.wdb.ExecContext(
		ctx,
		`
		INSERT INTO cache_blobs (target_addr, hashin, artifact_name, data, created_at)
		VALUES (?, ?, ?, ?, ?)
		ON CONFLICT(target_addr, hashin, artifact_name) DO UPDATE SET
			data       = excluded.data,
			created_at = excluded.created_at
		`,
		targetAddr, hashin, name, []byte{}, time.Now().UnixNano(),
	)
	if err != nil {
		return fmt.Errorf("writeEntry upsert: %w", err)
	}

	buf := c.pool.Get()
	defer c.pool.Put(buf)

	for {
		n, err := data.Read(buf)
		if n > 0 {
			_, errAppend := c.wdb.ExecContext(ctx, `
				UPDATE cache_blobs 
				SET data = data || ? 
				WHERE target_addr = ? AND hashin = ? AND artifact_name = ?
			`, buf[:n], targetAddr, hashin, name)
			if errAppend != nil {
				return fmt.Errorf("writeEntry append chunk: %w", errAppend)
			}
		}
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("writeEntry read: %w", err)
		}
	}

	return nil
}

type sqlCacheWriter struct {
	pw   *io.PipeWriter
	done <-chan error
}

func (w *sqlCacheWriter) Write(p []byte) (int, error) {
	return w.pw.Write(p)
}

func (w *sqlCacheWriter) Close() error {
	// Close the write end; this unblocks the goroutine's reader.
	if err := w.pw.Close(); err != nil {
		return err
	}
	// Wait for the goroutine to finish and return any write error.
	return <-w.done
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
		targetAddr := c.targetKey(ref)

		rows, err := c.rdb.QueryContext(ctx,
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
		targetAddr := c.targetKey(ref)

		rows, err := c.rdb.QueryContext(ctx,
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
