/// Port of the Go `generate_test_main.go` reference implementation.
///
/// Parses Go test source files using a simple line-by-line approach (no full AST),
/// then generates the `testmain.go` bootstrap file that the Go test framework requires.
use anyhow::Context;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFunc {
    pub package: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub package: String,
    pub name: String,
    pub output: String,
    pub unordered: bool,
}

/// Metadata extracted from test source files.
#[derive(Debug, Default)]
pub struct Analysis {
    pub tests: Vec<TestFunc>,
    pub benchmarks: Vec<TestFunc>,
    pub fuzz_targets: Vec<TestFunc>,
    pub examples: Vec<Example>,
    pub test_main: Option<TestFunc>,
    pub import_path: String,
    pub import_test: bool,
    pub import_xtest: bool,
    pub need_test: bool,
    pub need_xtest: bool,
    /// Always true for modern Go (≥1.18).
    pub is_go1_18: bool,
}

/// Parse a single Go test file and return extracted test functions.
///
/// `pkg_label` must be either `"_test"` (internal tests) or `"_xtest"` (external tests).
fn process_file(pkg_label: &str, path: &Path) -> anyhow::Result<PartialAnalysis> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(process_source(pkg_label, &content))
}

#[derive(Debug, Default)]
struct PartialAnalysis {
    tests: Vec<TestFunc>,
    benchmarks: Vec<TestFunc>,
    fuzz_targets: Vec<TestFunc>,
    // examples are omitted (require full AST)
    test_main: Option<TestFunc>,
    need_test: bool,
    need_xtest: bool,
}

/// Parse Go source text for test functions using a line-by-line heuristic.
///
/// The approach matches lines that begin with `func ` at column 0 (no leading
/// whitespace) and inspects the function name and parameter type.  This correctly
/// ignores method declarations (they have a receiver between `func` and the name)
/// and nested/indented functions.
fn process_source(pkg_label: &str, content: &str) -> PartialAnalysis {
    let mut result = PartialAnalysis::default();

    for line in content.lines() {
        // Only top-level (non-indented) `func` declarations.
        let Some(rest) = line.strip_prefix("func ") else {
            continue;
        };

        // Receiver check: `func (r *T) Name(` → skip (method)
        if rest.starts_with('(') {
            continue;
        }

        // Extract function name (everything up to '(').
        let Some((name_raw, params)) = rest.split_once('(') else {
            continue;
        };
        let name = name_raw.trim();
        if name.is_empty() {
            continue;
        }
        // Reconstruct params with the '(' prefix so existing helpers work.
        let params = &format!("({}", params);

        // Match TestMain first (special case).
        if name == "TestMain" {
            if params_match_type(params, "M") {
                result.test_main = Some(TestFunc {
                    name: name.to_string(),
                    package: pkg_label.to_string(),
                });
            } else if params_match_type(params, "T") {
                // TestMain(t *testing.T) — treated as a normal test
                result.tests.push(TestFunc {
                    name: name.to_string(),
                    package: pkg_label.to_string(),
                });
            }
            continue;
        }

        if is_test_name(name, "Test") && params_match_type(params, "T") {
            result.tests.push(TestFunc {
                name: name.to_string(),
                package: pkg_label.to_string(),
            });
        } else if is_test_name(name, "Benchmark") && params_match_type(params, "B") {
            result.benchmarks.push(TestFunc {
                name: name.to_string(),
                package: pkg_label.to_string(),
            });
        } else if is_test_name(name, "Fuzz") && params_match_type(params, "F") {
            result.fuzz_targets.push(TestFunc {
                name: name.to_string(),
                package: pkg_label.to_string(),
            });
        }
    }

    // Determine whether this set of findings belongs to test or xtest.
    let has_findings = !result.tests.is_empty()
        || !result.benchmarks.is_empty()
        || !result.fuzz_targets.is_empty()
        || result.test_main.is_some();

    if has_findings {
        match pkg_label {
            "_test" => result.need_test = true,
            "_xtest" => result.need_xtest = true,
            _ => {}
        }
    }

    result
}

/// Returns true if `name` looks like a test/benchmark/fuzz function with the given prefix.
///
/// Mirrors Go's heuristic: the character after the prefix (if any) must not be a lowercase letter.
fn is_test_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    if rest.is_empty() {
        return true; // bare "Test", "Benchmark", "Fuzz" are valid
    }
    // First character after prefix must not be lowercase.
    !rest.starts_with(|c: char| c.is_lowercase())
}

/// Returns true if the parameter list contains `*testing.T`, `*testing.B`, etc. for `type_char`.
///
/// We accept either `*T` or `*testing.T` (the import alias can vary, but we only care about
/// the selector name matching `type_char`).
fn params_match_type(params: &str, type_char: &str) -> bool {
    // params looks like `(t *testing.T)` or `(t *T)` or `(m *testing.M)` etc.
    // We just scan for `*T` or `*testing.T` anywhere inside.
    params.contains(&format!("*testing.{}", type_char))
        || params.contains(&format!("*{}", type_char))
}

/// Analyse a set of test files and produce a combined [`Analysis`].
///
/// `files` is a slice of `(prefix, absolute_path)` where `prefix` is `"_test"` (internal)
/// or `"_xtest"` (external).
pub fn analyze_test_main(import_path: &str, files: &[(&str, &str)]) -> anyhow::Result<Analysis> {
    let mut analysis = Analysis {
        import_path: import_path.to_string(),
        is_go1_18: true, // always assume modern Go
        ..Default::default()
    };

    for (prefix, path) in files {
        match *prefix {
            "_test" => analysis.import_test = true,
            "_xtest" => analysis.import_xtest = true,
            other => anyhow::bail!("unknown package prefix: {:?}", other),
        }

        let partial = process_file(prefix, Path::new(path))
            .with_context(|| format!("processing {}", path))?;

        analysis.tests.extend(partial.tests);
        analysis.benchmarks.extend(partial.benchmarks);
        analysis.fuzz_targets.extend(partial.fuzz_targets);

        if let Some(tm) = partial.test_main {
            anyhow::ensure!(
                analysis.test_main.is_none(),
                "multiple definitions of TestMain"
            );
            analysis.test_main = Some(tm);
        }

        analysis.need_test = analysis.need_test || partial.need_test;
        analysis.need_xtest = analysis.need_xtest || partial.need_xtest;
    }

    Ok(analysis)
}

/// Generate the `testmain.go` source text from an [`Analysis`].
///
/// Output matches the template in the reference Go implementation.
pub fn generate_testmain(analysis: &Analysis) -> String {
    let mut out = String::new();

    out.push_str("// Code generated by Heph for test binary. DO NOT EDIT.\n");
    out.push_str("package main\n");
    out.push_str("import (\n");
    out.push_str("\t\"os\"\n");
    if analysis.test_main.is_some() {
        out.push_str("\t\"reflect\"\n");
    }
    out.push_str("\t\"testing\"\n");
    out.push_str("\t\"testing/internal/testdeps\"\n");
    if analysis.import_test {
        let alias = if analysis.need_test { "_test" } else { "_" };
        out.push_str(&format!("\t{} {:?}\n", alias, analysis.import_path));
    }
    if analysis.import_xtest {
        let alias = if analysis.need_xtest { "_xtest" } else { "_" };
        let xtest_path = format!("{}_test", analysis.import_path);
        out.push_str(&format!("\t{} {:?}\n", alias, xtest_path));
    }
    out.push_str(")\n");
    out.push('\n');

    // tests slice
    out.push_str("var tests = []testing.InternalTest{\n");
    for tf in &analysis.tests {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}}},\n",
            tf.name, tf.package, tf.name
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // benchmarks slice
    out.push_str("var benchmarks = []testing.InternalBenchmark{\n");
    for tf in &analysis.benchmarks {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}}},\n",
            tf.name, tf.package, tf.name
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // examples slice
    out.push_str("var examples = []testing.InternalExample{\n");
    for ex in &analysis.examples {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}, {:?}, {}}},\n",
            ex.name, ex.package, ex.name, ex.output, ex.unordered
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // fuzz targets (Go ≥ 1.18)
    if analysis.is_go1_18 {
        out.push_str("var fuzzTargets = []testing.InternalFuzzTarget{\n");
        for tf in &analysis.fuzz_targets {
            out.push_str(&format!(
                "\t{{\"{}\" , {}.{}}},\n",
                tf.name, tf.package, tf.name
            ));
        }
        out.push_str("}\n");
        out.push('\n');
    }

    // init
    out.push_str("func init() {\n");
    out.push_str(&format!(
        "\ttestdeps.ImportPath = {:?}\n",
        analysis.import_path
    ));
    out.push_str("}\n");
    out.push('\n');

    // main
    out.push_str("func main() {\n");
    if analysis.is_go1_18 {
        out.push_str(
            "\tm := testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, fuzzTargets, examples)\n",
        );
    } else {
        out.push_str(
            "\tm := testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, examples)\n",
        );
    }
    if let Some(tm) = &analysis.test_main {
        out.push_str(&format!("\t{}.{}(m)\n", tm.package, tm.name));
        out.push_str("\tos.Exit(int(reflect.ValueOf(m).Elem().FieldByName(\"exitCode\").Int()))\n");
    } else {
        out.push_str("\tos.Exit(m.Run())\n");
    }
    out.push_str("}\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_file(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file_test.go");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn test_process_source_finds_test() {
        let source = "package pkg\n\nfunc TestFoo(t *testing.T) {}\n";
        let partial = process_source("_test", source);
        assert_eq!(partial.tests.len(), 1);
        assert_eq!(partial.tests[0].name, "TestFoo");
        assert_eq!(partial.tests[0].package, "_test");
    }

    #[test]
    fn test_process_source_ignores_testify_match() {
        // "Testify" starts with "Test" but next char is 'i' (lowercase) → skip
        let source = "package pkg\n\nfunc Testify(t *testing.T) {}\n";
        let partial = process_source("_test", source);
        assert!(partial.tests.is_empty());
    }

    #[test]
    fn test_process_source_finds_benchmark() {
        let source = "package pkg\n\nfunc BenchmarkFoo(b *testing.B) {}\n";
        let partial = process_source("_test", source);
        assert_eq!(partial.benchmarks.len(), 1);
        assert_eq!(partial.benchmarks[0].name, "BenchmarkFoo");
    }

    #[test]
    fn test_process_source_finds_fuzz() {
        let source = "package pkg\n\nfunc FuzzFoo(f *testing.F) {}\n";
        let partial = process_source("_test", source);
        assert_eq!(partial.fuzz_targets.len(), 1);
        assert_eq!(partial.fuzz_targets[0].name, "FuzzFoo");
    }

    #[test]
    fn test_process_source_finds_test_main() {
        let source = "package pkg\n\nfunc TestMain(m *testing.M) {}\n";
        let partial = process_source("_test", source);
        assert!(partial.test_main.is_some());
        assert_eq!(partial.test_main.as_ref().unwrap().name, "TestMain");
    }

    #[test]
    fn test_process_source_method_ignored() {
        // Method with receiver should be skipped
        let source = "package pkg\n\nfunc (s *Suite) TestFoo(t *testing.T) {}\n";
        let partial = process_source("_test", source);
        assert!(partial.tests.is_empty());
    }

    #[test]
    fn test_is_test_name_prefix_only() {
        assert!(is_test_name("Test", "Test"));
        assert!(is_test_name("Benchmark", "Benchmark"));
        assert!(is_test_name("Fuzz", "Fuzz"));
    }

    #[test]
    fn test_is_test_name_uppercase_suffix() {
        assert!(is_test_name("TestFoo", "Test"));
        assert!(is_test_name("TestHTTP", "Test"));
    }

    #[test]
    fn test_is_test_name_lowercase_suffix_rejected() {
        assert!(!is_test_name("Testify", "Test"));
        assert!(!is_test_name("Benchmarking", "Benchmark"));
    }

    #[test]
    fn test_analyze_test_main_empty() {
        let analysis = analyze_test_main("example.com/pkg", &[]).unwrap();
        assert!(analysis.tests.is_empty());
        assert!(!analysis.import_test);
        assert!(!analysis.import_xtest);
    }

    #[test]
    fn test_analyze_test_main_from_file() {
        let source = "package pkg\n\nfunc TestAdd(t *testing.T) {}\n";
        let (dir, path) = write_temp_file(source);
        let path_str = path.to_str().unwrap().to_string();
        drop(dir); // keep dir alive via path
        let _ = &path_str; // used below

        let source = "package pkg\n\nfunc TestAdd(t *testing.T) {}\n";
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pkg_test.go");
        std::fs::write(&p, source).unwrap();

        let files = vec![("_test", p.to_str().unwrap())];
        let analysis = analyze_test_main("example.com/pkg", &files).unwrap();
        assert_eq!(analysis.tests.len(), 1);
        assert_eq!(analysis.tests[0].name, "TestAdd");
        assert!(analysis.import_test);
        assert!(analysis.need_test);
    }

    #[test]
    fn test_generate_testmain_basic() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: true,
            tests: vec![TestFunc {
                name: "TestFoo".to_string(),
                package: "_test".to_string(),
            }],
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(out.contains("package main"));
        assert!(out.contains("TestFoo"));
        assert!(out.contains("_test"));
        assert!(out.contains("\"example.com/pkg\""));
        assert!(out.contains("testdeps.ImportPath"));
        assert!(out.contains("os.Exit(m.Run())"));
        assert!(out.contains("fuzzTargets")); // go1.18
    }

    #[test]
    fn test_generate_testmain_with_test_main_func() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: true,
            test_main: Some(TestFunc {
                name: "TestMain".to_string(),
                package: "_test".to_string(),
            }),
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(out.contains("reflect"));
        assert!(out.contains("_test.TestMain(m)"));
        assert!(out.contains("exitCode"));
        assert!(!out.contains("os.Exit(m.Run())"));
    }

    #[test]
    fn test_generate_testmain_no_go118() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: false,
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(!out.contains("fuzzTargets"));
        assert!(
            out.contains("testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, examples)")
        );
    }
}
