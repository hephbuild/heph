package asmtest

import "github.com/klauspost/cpuid/v2"

// Reference cpuid (which ships per-arch assembly) so its build_lib must run the
// asm/symabis/pack pipeline.
func Brand() string { return cpuid.CPU.BrandName }
