package utils

import (
	"encoding/hex"
	"fmt"
	"math/rand"
	"os"
	"time"
)

var ID string

func Seed() {
	i := int64(time.Now().Nanosecond())
	for _, arg := range os.Args {
		for _, r := range arg {
			i += int64(r)
		}
	}
	rand.Seed(i)
}

func RandStr(l int) string {
	randBytes := make([]byte, l)
	rand.Read(randBytes)

	return hex.EncodeToString(randBytes)
}

func init() {
	ID = fmt.Sprintf("%v", RandStr(10))
}
