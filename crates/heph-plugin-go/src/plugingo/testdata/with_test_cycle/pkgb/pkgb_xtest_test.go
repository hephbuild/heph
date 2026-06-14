// External (`package pkgb_test`) xtest IS allowed to cycle: pkgb_test is a
// distinct package from pkgb, so importing pkga (which imports pkgb) is
// permitted. This is the exact scenario that triggered the original
// fingerprint-mismatch bug: testmain links pkgb as build_test_lib, but pkga
// (a transitive importer of pkgb via xtest) was compiled against pkgb's
// normal build_lib — fingerprints differ. for_test_of=pkgb propagation makes
// pkga's recompile use build_test_lib, matching the linker's view.
package pkgb_test

import (
	"testing"

	"example.com/with_test_cycle/pkga"
	"example.com/with_test_cycle/pkgb"
)

func TestViaPkgA(t *testing.T) {
	if pkga.Wrap(pkgb.Value()) != 84 {
		t.Fatal("expected 84")
	}
}
