package plugingo

import (
	"github.com/hephbuild/heph/internal/hmaps"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go/build"
	"time"
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
	Deps             []string // Thats an std field
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

func (p Package) GetBuildLibTargetRef(main bool) *pluginv1.TargetRef {
	if p.LibTargetRef != nil {
		return p.LibTargetRef
	}

	args := p.Factors.Args()
	if main {
		args = hmaps.Concat(args, map[string]string{"main": "true"})
	}

	return &pluginv1.TargetRef{
		Package: p.GetHephBuildPackage(),
		Name:    "build_lib",
		Args:    args,
	}
}
