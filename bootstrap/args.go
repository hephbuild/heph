package bootstrap

type RunOpts struct {
	NoInline    bool    // Run all targets as a background job
	Plain       bool    // Disable TUI
	PrintOutput BoolStr // Print output paths
	CatOutput   BoolStr // Print output content
	NoCache     bool    // Disable cache
}

type BoolStr struct {
	Bool bool
	Str  string
}
