package hfs

import (
	"context"
	"errors"
	iofs "io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Equivalent to `strings.HasPrefix(path, prefix+"/")`, without the string concat.
func matchPrefix(path, prefix string) bool {
	return path == prefix || len(path) >= len(prefix) &&
		strings.HasPrefix(path, prefix) &&
		path[len(prefix)] == '/'
}

func fastMatchDir(path, matcher string) bool {
	i := indexMeta(matcher)
	if i == -1 {
		if matchPrefix(path, matcher) {
			return true
		}
	}

	return false
}

func PathMatchAny(path string, matchers ...string) (bool, error) {
	if len(matchers) == 0 {
		return false, nil
	}

	path = filepath.Clean(path)

	for _, matcher := range matchers {
		matcher = filepath.Clean(matcher)

		if strings.HasSuffix(matcher, "/**/*") {
			matcher := strings.TrimSuffix(matcher, "/**/*")

			if fastMatchDir(path, matcher) {
				return true, nil
			}
		}

		if fastMatchDir(path, matcher) {
			return true, nil
		}

		match, err := doublestar.PathMatch(matcher, path)
		if match || err != nil {
			return match, err
		}
	}

	return false, nil
}

// From doublestar package

var metaReplacer = strings.NewReplacer("\\*", "*", "\\?", "?", "\\[", "[", "\\]", "]", "\\{", "{", "\\}", "}")

// Unescapes meta characters (*?[]{}).
func unescapeMeta(pattern string) string {
	return metaReplacer.Replace(pattern)
}

func indexMeta(s string) int {
	var c byte
	l := len(s)
	for i := 0; i < l; i++ {
		c = s[i]
		switch c {
		case '*', '?', '[', '{':
			return i
		case '\\':
			// skip next byte
			i++
		}
	}
	return -1
}

func IsGlob(path string) bool {
	return indexMeta(path) != -1
}

func GlobSplit(p string) (base, pattern string) { //nolint:nonamedreturns
	return doublestar.SplitPattern(p)
}

type GlobEntry struct {
	RelPath string
	Node    RONode
}

type GlobWalkFunc func(entry GlobEntry) error

type GlobOption = func(*globConfig)

type globConfig struct {
	strictDir bool
}

func WithStrictDir(strictDir bool) GlobOption {
	return func(g *globConfig) {
		g.strictDir = strictDir
	}
}

func Glob(ctx context.Context, node RONode, pattern string, ignore []string, fn GlobWalkFunc, options ...GlobOption) error {
	return glob(ctx, node, pattern, ignore, fn, options...)
}

var ErrStrictDir = errors.New("strict directory is enabled, but pattern doesnt allow directory")

func glob(ctx context.Context, node RONode, pattern string, ignore []string, fn GlobWalkFunc, options ...GlobOption) error {
	config := &globConfig{}
	for _, option := range options {
		option(config)
	}

	prefix := ""

	i := indexMeta(pattern)
	if i == -1 {
		info, err := node.AtRO(unescapeMeta(pattern)).Lstat()
		if err != nil {
			if errors.Is(err, ErrNotExist) {
				return nil
			}

			return err
		}

		if config.strictDir {
			if strings.HasSuffix(pattern, "/") != info.IsDir() {
				return ErrStrictDir
			}
		}

		if info.IsDir() {
			prefix = pattern
			pattern = "**/*"
		}
	} else {
		prefix, pattern = GlobSplit(pattern)
		if prefix == "." {
			prefix = ""
		}
	}

	walkNode := node
	if prefix != "" {
		walkNode = At(node, prefix)
	}

	if pattern == "**/*" {
		return globAll(ctx, node, walkNode, prefix, ignore, fn)
	}

	return doublestar.GlobWalk(ToIOFS(walkNode), pattern, func(path string, d iofs.DirEntry) error {
		return innerGlob(ctx, node, filepath.Join(prefix, path), ignore, d, fn)
	})
}

func globAll(ctx context.Context, rootNode RONode, node RONode, prefix string, ignore []string, fn GlobWalkFunc) error {
	return globAllWalk(ctx, rootNode, node, node, prefix, ignore, fn)
}

func globAllWalk(ctx context.Context, globRootNode RONode, rootNode RONode, walkNode RONode, prefix string, ignore []string, fn GlobWalkFunc) error {
	return iofs.WalkDir(ToIOFS(walkNode), ".", func(relPath string, d DirEntry, err error) error {
		if err != nil {
			if errors.Is(err, iofs.ErrNotExist) {
				return nil
			}

			return err
		}

		// "." is the root of the walkNode; skip it (it has no meaningful path-based name)
		if relPath == "." {
			return nil
		}

		fullpath := filepath.Join(prefix, relPath)

		if d.Type()&iofs.ModeSymlink != 0 {
			// iofs.WalkDir does not descend into symlink directories.
			// Resolve the symlink target and handle manually.
			info, err := rootNode.AtRO(fullpath).Stat()
			if err != nil {
				return err
			}

			if info.IsDir() {
				// Recurse into the symlink target as if it were a regular directory.
				return globAllWalk(ctx, globRootNode, rootNode, walkNode.AtRO(relPath), fullpath, ignore, fn)
			}
		}

		return innerGlob(ctx, globRootNode, fullpath, ignore, d, fn)
	})
}

func innerGlob(ctx context.Context, rootNode RONode, path string, ignore []string, d iofs.DirEntry, fn GlobWalkFunc) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	if d.IsDir() {
		skip, err := PathMatchAny(path, ignore...)
		if err != nil {
			return err
		}

		if skip {
			return filepath.SkipDir
		}
		// Only match files
		return nil
	}

	skip, err := PathMatchAny(path, ignore...)
	if err != nil {
		return err
	}

	if skip {
		return nil
	}

	err = fn(GlobEntry{
		RelPath: path,
		Node:    withInfo(rootNode.AtRO(path), d.Info),
	})
	if err != nil {
		return err
	}

	return nil
}

type rONodeWithInfoFunc struct {
	RONode

	infoFunc func() (iofs.FileInfo, error)
}

func (r rONodeWithInfoFunc) Stat() (FileInfo, error) {
	return r.infoFunc()
}

func withInfo(ro RONode, info func() (iofs.FileInfo, error)) RONode {
	return &rONodeWithInfoFunc{ro, info}
}

func IsExecOwner(mode FileMode) bool {
	return mode&0100 != 0
}
