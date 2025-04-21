package plugingo

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestRoundtrip1(t *testing.T) {
	pkg := ThirdpartyBuildPackage("go/mod-thirdparty", "github.com/foo/bar", "v1.2.3", "maps")
	basePkg, goMod, version, pkgPath, ok := ParseThirdpartyPackage(pkg)
	assert.True(t, ok)
	assert.Equal(t, "go/mod-thirdparty", basePkg)
	assert.Equal(t, "github.com/foo/bar", goMod)
	assert.Equal(t, "v1.2.3", version)
	assert.Equal(t, "maps", pkgPath)
}

func TestRoundtrip2(t *testing.T) {
	pkg := ThirdpartyContentPackage("github.com/foo/bar", "v1.2.3", "maps")
	basePkg, goMod, version, pkgPath, ok := ParseThirdpartyPackage(pkg)
	assert.True(t, ok)
	assert.Equal(t, "", basePkg)
	assert.Equal(t, "github.com/foo/bar", goMod)
	assert.Equal(t, "v1.2.3", version)
	assert.Equal(t, "maps", pkgPath)
}
