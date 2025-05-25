package main

import (
	"capnproto.org/go/capnp/v3/rpc"
	"context"
	"fmt"
	arith "github.com/hephbuild/heph/internal/experiment/capnp/capnp"
	"io"
)

func Request(ctx context.Context, rwc io.ReadWriteCloser) error {
	conn := rpc.NewConn(rpc.NewStreamTransport(rwc), nil)
	defer conn.Close()

	// Now we resolve the bootstrap interface from the remote ArithServer.
	// Thanks to Cap'n Proto's promise pipelining, this function call does
	// NOT block.  We can start making RPC calls with 'a' immediately, and
	// these will transparently resolve when bootstrapping completes.
	//
	// The context can be used to time-out or otherwise abort the bootstrap
	// call.   It is safe to cancel the context after the first method call
	// on 'a' completes.
	a := arith.Arith(conn.Bootstrap(ctx))

	// Okay! Let's make an RPC call!  Remember:  RPC is performed simply by
	// calling a's methods.
	//
	// There are couple of interesting things to note here:
	//  1. We pass a callback function to set parameters on the RPC call.  If the
	//     call takes no arguments, you MAY pass nil.
	//  2. We return a Future type, representing the in-flight RPC call.  As with
	//     the earlier call to Bootstrap, a's methods do not block.  They instead
	//     return a future that eventually resolves with the RPC results. We also
	//     return a release function, which MUST be called when you're done with
	//     the RPC call and its results.
	f, release := a.Multiply(ctx, func(ps arith.Arith_multiply_Params) error {
		ps.SetA(2)
		ps.SetB(42)
		return nil
	})
	defer release()

	// You can do other things while the RPC call is in-flight.  Everything
	// is asynchronous. For simplicity, we're going to block until the call
	// completes.
	res, err := f.Struct()
	if err != nil {
		return err
	}

	fmt.Println("GOT RESULT", res.Product()) // prints 84

	return nil
}
