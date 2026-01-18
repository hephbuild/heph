package pluginnix

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"text/template"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

// Nix expression template for creating wrapper binaries
const nixWrapperTemplate = `
with import <nixpkgs> {};
let
  packages = [ {{range .Packages}}{{.}} {{end}}];
  
  # Discover all binaries across all packages
  allBins = lib.unique (lib.concatMap 
    (pkg: 
      if builtins.pathExists "${pkg}/bin" 
      then builtins.attrNames (builtins.readDir "${pkg}/bin")
      else []
    ) 
    packages
  );
  
  # Create wrapper for a single binary
  makeWrapper = bin: writeTextFile {
    name = bin;
    executable = true;
    destination = "/bin/${bin}";
    text = ''
      #!/usr/bin/env sh
      export PATH=${lib.makeBinPath packages}:$PATH
      export PKG_CONFIG_PATH="${lib.makeSearchPath "lib/pkgconfig" packages}"
      export CMAKE_PREFIX_PATH="${lib.concatStringsSep ":" packages}"
      export NIX_CFLAGS_COMPILE="${lib.concatStringsSep " " (map (p: "-isystem ${p}/include") (lib.filter (p: p ? include) packages))}"
      export NIX_LDFLAGS="${lib.concatStringsSep " " (map (p: "-L${p}/lib") (lib.filter (p: p ? lib) packages))}"
      exec ${bin} "$@"
    '';
  };
in
# Create a directory with all wrappers combined
buildEnv {
  name = "{{.Name}}-wrappers";
  paths = map makeWrapper allBins;
}
`

type nixWrapperData struct {
	Name     string
	Packages []string
	NixRef   string
}

// generateWrapperNix creates the Nix expression for building wrapper scripts
func generateWrapperNix(nixRef string, packages []string, wrapperName string) (string, error) {
	if len(packages) == 0 {
		return "", fmt.Errorf("packages list cannot be empty")
	}

	// Convert package names to nix package references
	nixPackages := make([]string, len(packages))
	for i, pkg := range packages {
		nixPackages[i] = pkg
	}

	data := nixWrapperData{
		Name:     wrapperName,
		Packages: nixPackages,
		NixRef:   nixRef,
	}

	tmpl, err := template.New("wrapper").Parse(nixWrapperTemplate)
	if err != nil {
		return "", fmt.Errorf("failed to parse template: %w", err)
	}

	var buf bytes.Buffer
	if err := tmpl.Execute(&buf, data); err != nil {
		return "", fmt.Errorf("failed to execute template: %w", err)
	}

	return buf.String(), nil
}

// runNixBuild executes nix-build with the given Nix expression and returns the store path
func runNixBuild(ctx context.Context, nixExpr string) (string, error) {
	// Create temporary file for the nix expression
	tmpFile, err := os.CreateTemp("", "nix-wrapper-*.nix")
	if err != nil {
		return "", fmt.Errorf("failed to create temp file: %w", err)
	}
	defer os.Remove(tmpFile.Name())

	if _, err := tmpFile.WriteString(nixExpr); err != nil {
		tmpFile.Close()
		return "", fmt.Errorf("failed to write nix expression: %w", err)
	}
	tmpFile.Close()

	// Run nix-build
	cmd := exec.CommandContext(ctx, "nix-build", tmpFile.Name(), "--no-out-link")
	cmd.Env = append(os.Environ(), "NIX_CONFIG="+nixConfig)

	output, err := cmd.Output()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			return "", fmt.Errorf("nix-build failed: %w\nstderr: %s", err, exitErr.Stderr)
		}
		return "", fmt.Errorf("nix-build failed: %w", err)
	}

	storePath := strings.TrimSpace(string(output))
	return storePath, nil
}

// discoverBinaries lists all executables in a Nix store path
func discoverBinaries(storePath string) ([]string, error) {
	binDir := filepath.Join(storePath, "bin")

	entries, err := os.ReadDir(binDir)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil // No bin directory
		}
		return nil, fmt.Errorf("failed to read bin directory: %w", err)
	}

	var binaries []string
	for _, entry := range entries {
		if !entry.IsDir() {
			info, err := entry.Info()
			if err != nil {
				continue
			}
			// Check if executable
			if info.Mode()&0111 != 0 {
				binaries = append(binaries, entry.Name())
			}
		}
	}

	return binaries, nil
}

// createWrapperTargets creates target specs for each discovered binary
func createWrapperTargets(nixRef, nixPkg string, storePath string, binaries []string, nixToolRef *tref.Ref, hash bool) ([]*pluginv1.GetResponse, error) {
	if len(binaries) == 0 {
		return nil, fmt.Errorf("no binaries found in wrapper")
	}

	responses := make([]*pluginv1.GetResponse, 0, len(binaries))

	for _, binary := range binaries {
		args := map[string]string{
			"wrapper": "1",
			"bin":     binary,
		}
		if hash {
			args["hash"] = "1"
		} else {
			args["hash"] = "0"
		}

		ref := tref.New(tref.JoinPackage("@nix", nixRef), nixPkg, args)
		wrapperPath := filepath.Join(storePath, "bin", binary)

		// Create a target that copies the wrapper to the output
		resp := pluginv1.GetResponse_builder{
			Spec: pluginv1.TargetSpec_builder{
				Ref:    ref,
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run": structpb.NewStringValue(fmt.Sprintf("cp %s $OUT && chmod +x $OUT", wrapperPath)),
					"env": structpb.NewStructValue(&structpb.Struct{
						Fields: map[string]*structpb.Value{
							"NIX_CONFIG": structpb.NewStringValue(nixConfig),
						},
					}),
					"out": structpb.NewStringValue(binary),
				},
				Transitive: pluginv1.Sandbox_builder{
					Tools: []*pluginv1.Sandbox_Tool{
						pluginv1.Sandbox_Tool_builder{
							Ref:  tref.WithOut(nixToolRef, ""),
							Hash: htypes.Ptr(false),
						}.Build(),
					},
				}.Build(),
			}.Build(),
		}.Build()

		responses = append(responses, resp)
	}

	return responses, nil
}
