package plugingo

import (
	"go/build"
	"time"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hmaps"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

const MainPackage = "main"

type Module struct {
	Path       string       // module path
	Query      string       // version query corresponding to this version
	Version    string       // module version
	Versions   []string     // available module versions
	Replace    *Module      // replaced by this module
	Time       *time.Time   // time version was created
	Update     *Module      // available update (with -u)
	Main       bool         // is this the main module?
	Indirect   bool         // module is only indirectly needed by main module
	Dir        string       // directory holding local copy of files, if any
	GoMod      string       // path to go.mod file describing module, if any
	GoVersion  string       // go version used in module
	Retracted  []string     // retraction information, if any (with -retracted or -u)
	Deprecated string       // deprecation message, if any (with -u)
	Error      *ModuleError // error loading module
	Sum        string       // checksum for path, version (as in go.sum)
	GoModSum   string       // checksum for go.mod (as in go.sum)
	Origin     any          // provenance of module
	Reuse      bool         // reuse of old module info is safe

	HephPackage string
}

type ModuleError struct {
	Err string // the error itself
}

type Package struct {
	build.Package
	Factors          Factors
	Module           *Module
	HephPackage      string
	HephBuildPackage string
	IsStd            bool
	Is3rdParty       bool
	LibTargetRef     *pluginv1.TargetRef
}

func (p Package) HasTest() bool {
	return len(p.TestGoFiles) > 0
}
func (p Package) HasXTest() bool {
	return len(p.XTestGoFiles) > 0
}
func (p Package) GetHephBuildPackage() string {
	if p.HephBuildPackage != "" {
		return p.HephBuildPackage
	}

	return p.HephPackage
}

func (p Package) GetBuildImportPath(main bool) string {
	if main && p.IsCommand() {
		return MainPackage
	} else {
		return p.ImportPath
	}
}

func (p Package) GetBuildLibTargetRef(mode string) *pluginv1.TargetRef {
	if p.LibTargetRef != nil {
		return p.LibTargetRef
	}

	args := p.Factors.Args()
	if mode != "" {
		args = hmaps.Concat(args, map[string]string{"mode": mode})
	}

	return pluginv1.TargetRef_builder{
		Package: htypes.Ptr(p.GetHephBuildPackage()),
		Name:    htypes.Ptr("build_lib"),
		Args:    args,
	}.Build()
}
