// A generator built by heph itself: the codegen target that produces the embedded
// asset runs THIS binary, so the asset's target transitively depends on a go
// target (and thus on that package's `_golist`).
package main

import "fmt"

func main() {
	fmt.Println("openapi: generated")
}
