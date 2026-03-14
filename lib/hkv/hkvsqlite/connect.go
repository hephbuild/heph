package hkvsqlite

import (
	"context"
	"database/sql/driver"
	"fmt"
	"strings"

	"modernc.org/sqlite"
)

type dsnConnector struct {
	dsn    string
	driver driver.Driver
}

func (t dsnConnector) Connect(_ context.Context) (driver.Conn, error) {
	return t.driver.Open(t.dsn)
}

func (t dsnConnector) Driver() driver.Driver {
	return t.driver
}

type ConnConfig struct {
	CacheSize int64
	PageSize  int64
	MmapSize  int64
}

func newConnector(dsn string, cfg ConnConfig) dsnConnector {
	d := &sqlite.Driver{}

	var sb strings.Builder
	sb.WriteString(`
		PRAGMA journal_mode = WAL;
		PRAGMA busy_timeout = 10000;
		PRAGMA synchronous = NORMAL;
		PRAGMA foreign_keys = ON;
		PRAGMA temp_store = MEMORY;
		PRAGMA auto_vacuum = INCREMENTAL;
	`)
	if cfg.PageSize != 0 {
		_, _ = fmt.Fprintf(&sb, "PRAGMA page_size = %d;", cfg.PageSize)
	}
	if cfg.CacheSize != 0 {
		// `-` denotes kibibytes
		_, _ = fmt.Fprintf(&sb, "PRAGMA cache_size = -%d;", cfg.CacheSize)
	}
	if cfg.MmapSize != 0 {
		_, _ = fmt.Fprintf(&sb, "PRAGMA mmap_size = %d;", cfg.MmapSize)
	}
	s := sb.String()

	d.RegisterConnectionHook(func(conn sqlite.ExecQuerierContext, _ string) error {
		_, err := conn.ExecContext(context.Background(), s, nil)
		return err
	})

	return dsnConnector{
		dsn:    dsn,
		driver: d,
	}
}
