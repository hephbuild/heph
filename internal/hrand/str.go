package hrand

import (
	"crypto/rand"
	"encoding/hex"
)

func Str(l int) string {
	randBytes := make([]byte, l)
	_, err := rand.Read(randBytes)
	if err != nil {
		panic(err)
	}

	return hex.EncodeToString(randBytes)
}
