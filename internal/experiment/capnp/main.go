package main

import (
	"context"
	"github.com/hephbuild/heph/herrgroup"
	"log"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

func main() {
	exe, err := os.Executable()
	if err != nil {
		panic(err)
	}

	ctx := context.Background()

	args := os.Args[1:]

	if len(args) == 0 {
		var g herrgroup.Group

		dir, err := os.MkdirTemp("", "")
		if err != nil {
			panic(err)
		}
		defer os.RemoveAll(dir)

		f := filepath.Join(dir, "sock")

		g.Go(func() error {
			cmd := exec.CommandContext(ctx, exe, "server", f)
			cmd.Stdout = os.Stdout
			cmd.Stderr = os.Stderr

			return cmd.Run()
		})

		g.Go(func() error {
			var i int
			for {
				cmd := exec.CommandContext(ctx, exe, "client", f)
				cmd.Stdout = os.Stdout
				cmd.Stderr = os.Stderr

				err := cmd.Run()
				if err == nil {
					return nil
				}

				i++

				if i > 5 {
					return err
				}

				time.Sleep(time.Millisecond)
			}
		})

		err = g.Wait()
		if err != nil {
			panic(err)
		}

		return
	}

	if len(args) != 2 {
		panic("no command")
	}

	switch args[0] {
	case "server":
		addr := args[1]

		l, err := net.Listen("unix", addr)
		if err != nil {
			panic(err)
		}

		defer func(l net.Listener) {
			err := l.Close()
			if err != nil {
				panic(err)
			}
		}(l)

		go func() {
			c, err := l.Accept()
			if err != nil {
				log.Fatal(err)
			}

			err = Serve(ctx, c)
			if err != nil {
				panic(err)
			}
		}()

		<-ctx.Done()
	case "client":
		addr := args[1]

		c, err := net.Dial("unix", addr)
		if err != nil {
			log.Fatal(err)
		}

		err = Request(ctx, c)
		if err != nil {
			panic(err)
		}
	default:
		panic("unknown command")
	}

}
