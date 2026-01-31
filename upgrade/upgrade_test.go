package upgrade

import (
	"bytes"
	"io"
	"net/http"
	"testing"

	"github.com/stretchr/testify/assert"
)

func TestFindLatestVersion(t *testing.T) {
	t.Skip("Skip network test")

	v, err := findLatestVersion()
	assert.NoError(t, err)
	assert.NotEmpty(t, v)

	t.Logf("Latest version: %v", v)

	res, err := http.Get("https://storage.googleapis.com/heph-build/latest_version")
	assert.NoError(t, err)

	latest, err := io.ReadAll(res.Body)
	assert.NoError(t, err)

	gcsLatest := string(bytes.TrimSpace(latest))
	assert.NotEmpty(t, gcsLatest)

	t.Logf("GCS latest version: %v", gcsLatest)

	assert.Equal(t, gcsLatest, v)
}
