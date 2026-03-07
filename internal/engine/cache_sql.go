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

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	_ "modernc.org/sqlite"
)

type SQLCache struct {
	db   *sql.DB
	root hfs.OS
}

func (c *SQLCache) Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error) {
	targetAddr := c.targetKey(ref)

	var count int
	err := c.db.QueryRowContext(
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
		_, err = c.db.ExecContext(
			ctx,
			`DELETE FROM cache_blobs WHERE target_addr = ? AND hashin = ?`,
			targetAddr, hashin,
		)
	} else {
		_, err = c.db.ExecContext(
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

func OpenSQLCacheDB(path string) (*sql.DB, error) {
	if err := os.MkdirAll(filepath.Dir(path), os.ModePerm); err != nil {
		return nil, fmt.Errorf("OpenSQLCacheDB mkdir: %w", err)
	}

	dsn := path +
		"?_pragma=journal_mode(WAL)" +
		"&_pragma=busy_timeout(10000)" +
		"&_pragma=synchronous(NORMAL)" +
		"&_pragma=foreign_keys(ON)"

	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("OpenSQLCacheDB open: %w", err)
	}

	if err := migrateSQLCacheDB(db); err != nil {
		_ = db.Close()
		return nil, fmt.Errorf("OpenSQLCacheDB migrate: %w", err)
	}

	return db, nil
}

func NewSQLCache(db *sql.DB) *SQLCache {
	return &SQLCache{
		db: db,
	}
}

// targetAddr computes a stable filesystem-safe address for a TargetRef.
// When the ref has no args, this is just "__<name>". When args are present,
// a hash of the full ref is appended to disambiguate.
func (c *SQLCache) targetAddr(ref *pluginv1.TargetRef) string {
	if len(ref.GetArgs()) == 0 {
		return "__" + ref.GetName()
	}

	h := xxh3.New()
	hashpb.Hash(h, ref, tref.OmitHashPb)

	return "__" + ref.GetName() + "_" + hex.EncodeToString(h.Sum(nil))
}

// targetKey returns the compound target address used as target_addr in the DB.
func (c *SQLCache) targetKey(ref *pluginv1.TargetRef) string {
	return ref.GetPackage() + "/" + c.targetAddr(ref)
}

func (c *SQLCache) Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error) {
	targetAddr := c.targetKey(ref)

	var dataBytes sql.NullString

	err := c.db.QueryRowContext(
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

type sqlWriter struct {
	pw     *io.PipeWriter
	errC   chan error
	closed bool
}

func (w *sqlWriter) Write(p []byte) (n int, err error) {
	return w.pw.Write(p)
}

func (w *sqlWriter) Close() error {
	if w.closed {
		return nil
	}
	w.closed = true

	err := w.pw.Close()
	writeErr := <-w.errC
	if writeErr != nil {
		return writeErr
	}
	return err
}

func (c *SQLCache) writeEntry(ctx context.Context, targetAddr, hashin, name string, data io.Reader) error {
	_, err := c.db.ExecContext(
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
		return fmt.Errorf("writeEntry upsert %w", err)
	}

	buf := make([]byte, 32*1024) // 32KB chunks
	for {
		n, err := data.Read(buf)
		if n > 0 {
			_, errAppend := c.db.ExecContext(ctx, `
				UPDATE cache_blobs 
				SET data = data || ? 
				WHERE target_addr = ? AND hashin = ? AND artifact_name = ?
			`, buf[:n], targetAddr, hashin, name)
			if errAppend != nil {
				return fmt.Errorf("writeEntry append chunk %w", errAppend)
			}
		}
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("writeEntry read %w", err)
		}
	}

	return nil
}

func (c *SQLCache) Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error) {
	targetAddr := c.targetKey(ref)

	pr, pw := io.Pipe()

	errC := make(chan error, 1)

	go func() {
		defer pr.Close()
		err := c.writeEntry(ctx, targetAddr, hashin, name, pr)
		if err != nil {
			err = fmt.Errorf("writer write: %q %q %q %w", tref.Format(ref), hashin, name, err)
		}
		errC <- err
	}()

	return &sqlWriter{
		pw:   pw,
		errC: errC,
	}, nil
}

func (c *SQLCache) ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		targetAddr := c.targetKey(ref)

		rows, err := c.db.QueryContext(ctx,
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

func (c *SQLCache) ListVersions(ctx context.Context, ref *pluginv1.TargetRef, name string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		targetAddr := c.targetKey(ref)

		rows, err := c.db.QueryContext(ctx,
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
