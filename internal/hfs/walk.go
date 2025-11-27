package hfs

import (
	sync_map "github.com/zolstein/sync-map"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

type CachedEntry struct {
	info fs.FileInfo
}

func (c *CachedEntry) Name() string               { return c.info.Name() }
func (c *CachedEntry) IsDir() bool                { return c.info.IsDir() }
func (c *CachedEntry) Type() fs.FileMode          { return c.info.Mode().Type() }
func (c *CachedEntry) Info() (fs.FileInfo, error) { return c.info, nil }

// Node represents a file or directory in our memory tree.
type Node struct {
	Path     string
	Entry    fs.DirEntry
	Children []*Node
	scanned  bool // true if we have read the directory from disk
}

// FSCache holds a map of all loaded paths in the file system.
type FSCache struct {
	nodes sync_map.Map[string, *Node]
}

// NewFSCache initializes a blank cache.
func NewFSCache() *FSCache {
	return &FSCache{}
}

// Walk simulates fs.WalkDir using the in-memory cache.
// It accepts a root path, allowing you to walk any directory.
func (c *FSCache) Walk(root string, fn fs.WalkDirFunc) error {
	root = filepath.Clean(root)

	// 1. Get or Create the starting node
	node, exists := c.nodes.Load(root)
	if !exists {
		// We haven't seen this path before, so we must Lstat it
		info, err := os.Lstat(root)
		if err != nil {
			return fn(root, nil, err)
		}

		node = &Node{
			Path:    root,
			Entry:   &CachedEntry{info: info},
			scanned: false,
		}
		c.nodes.Store(root, node)
	}

	return c.walkRecursive(node, fn)
}

func (c *FSCache) walkRecursive(node *Node, fn fs.WalkDirFunc) error {
	// 1. Visit the node itself
	err := fn(node.Path, node.Entry, nil)

	// Handle SkipDir
	if err == fs.SkipDir {
		if node.Entry.IsDir() {
			return nil // Skip processing children
		}
		return nil
	}
	if err != nil {
		return err
	}

	// If it's a file, we are done
	if !node.Entry.IsDir() {
		return nil
	}

	// 2. Ensure children are loaded (On-Demand Logic)
	if !node.scanned {
		// Hit the disk
		entries, readErr := os.ReadDir(node.Path)

		if readErr != nil {
			err = fn(node.Path, node.Entry, readErr)
			if err != nil {
				if err == fs.SkipDir {
					return nil
				}
				return err
			}
			return nil
		}

		// Hydrate children into cache
		node.Children = make([]*Node, 0, len(entries))
		for _, e := range entries {
			childPath := filepath.Join(node.Path, e.Name())

			// Check if we already have this child in our global map
			// (This handles cases where we walked a subdirectory before the parent)
			childNode, exists := c.nodes.Load(childPath)

			if !exists {
				info, err := e.Info()
				if err != nil {
					continue
				}

				childNode = &Node{
					Path:  childPath,
					Entry: &CachedEntry{info: info},
				}
				// Register in the global map
				c.nodes.Store(childPath, childNode)
			}

			node.Children = append(node.Children, childNode)
		}

		// Sort to maintain deterministic order
		slices.SortFunc(node.Children, func(i, j *Node) int {
			return strings.Compare(i.Entry.Name(), j.Entry.Name())
		})

		node.scanned = true
	}

	// 3. Recurse into children (using memory cache)
	for _, child := range node.Children {
		if err := c.walkRecursive(child, fn); err != nil {
			if err == fs.SkipDir {
				continue
			}
			return err
		}
	}

	return nil
}
