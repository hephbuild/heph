package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/hanwen/go-fuse/v2/fs"
	"github.com/hanwen/go-fuse/v2/fuse"
)

// files contains the files we will expose as a file system
var files = map[string]string{
	"file":              "content",
	"subdir/other-file": "other-content",
}

// inMemoryFS is the root of the tree
type inMemoryFS struct {
	fs.Inode
}

// Ensure that we implement NodeOnAdder
var _ = (fs.NodeOnAdder)((*inMemoryFS)(nil))

// OnAdd is called on mounting the file system. Use it to populate
// the file system tree.
func (root *inMemoryFS) OnAdd(ctx context.Context) {
	for name, content := range files {
		dir, base := filepath.Split(name)

		p := &root.Inode

		// Add directories leading up to the file.
		for _, component := range strings.Split(dir, "/") {
			if len(component) == 0 {
				continue
			}
			ch := p.GetChild(component)
			if ch == nil {
				// Create a directory
				ch = p.NewPersistentInode(ctx, &fs.Inode{},
					fs.StableAttr{Mode: syscall.S_IFDIR})
				// Add it
				p.AddChild(component, ch, true)
			}

			p = ch
		}

		// Make a file out of the content bytes. This type
		// provides the open/read/flush methods.
		embedder := &fs.MemRegularFile{
			Data: []byte(content),
		}

		// Create the file. The Inode must be persistent,
		// because its life time is not under control of the
		// kernel.
		child := p.NewPersistentInode(ctx, embedder, fs.StableAttr{})

		// And add it
		p.AddChild(base, child, true)
	}
}

// This demonstrates how to build a file system in memory. The
// read/write logic for the file is provided by the MemRegularFile type.
func main() {
	// This is where we'll mount the FS
	mntDir, _ := os.MkdirTemp("", "")

	root := &inMemoryFS{}
	start := time.Now()
	server, err := fs.Mount(mntDir, root, &fs.Options{
		MountOptions: fuse.MountOptions{Debug: false},
	})
	fmt.Println(time.Since(start))
	if err != nil {
		log.Panic(err)
	}
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()
	go func() {
		<-ctx.Done()
		err := server.Unmount()
		if err != nil {
			log.Printf("unmount failed: %v", err)
		}
	}()

	log.Printf("Mounted on %s", mntDir)

	// Wait until unmount before exiting
	server.Wait()
}
