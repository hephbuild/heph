//! Build file parsing

use crate::{Result, StarlarkRuntime, TargetData};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct BuildFile {
    pub path: String,
    pub targets: Vec<BuildTarget>,
    pub package: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BuildTarget {
    pub name: String,
    pub deps: Vec<String>,
    pub srcs: Vec<String>,
    pub outs: Vec<String>,
}

impl From<TargetData> for BuildTarget {
    fn from(data: TargetData) -> Self {
        Self {
            name: data.name,
            deps: data.deps,
            srcs: data.srcs,
            outs: data.outs,
        }
    }
}

pub struct BuildFileParser {
    runtime: StarlarkRuntime,
}

impl BuildFileParser {
    pub fn new() -> Self {
        Self {
            runtime: StarlarkRuntime::new(),
        }
    }

    /// Parse a build file
    pub fn parse<P: AsRef<Path>>(&self, path: P) -> Result<BuildFile> {
        let path_str = path.as_ref().to_str().unwrap();

        // Evaluate the build file
        self.runtime.eval_file(path_str)?;

        // Get collected data
        let collected = self.runtime.get_collected();

        Ok(BuildFile {
            path: path_str.to_string(),
            targets: collected.targets.into_iter().map(|t| t.into()).collect(),
            package: collected.package_name,
        })
    }

    /// Parse build file content directly (for testing)
    pub fn parse_content(&self, filename: &str, content: &str) -> Result<BuildFile> {
        self.runtime.eval(filename, content)?;

        let collected = self.runtime.get_collected();

        Ok(BuildFile {
            path: filename.to_string(),
            targets: collected.targets.into_iter().map(|t| t.into()).collect(),
            package: collected.package_name,
        })
    }
}

impl Default for BuildFileParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_build_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
package(name="test_pkg")

target(
    name="test_target",
    deps=["dep1"]
)
"#
        )
        .unwrap();
        file.flush().unwrap();

        let parser = BuildFileParser::new();
        let build_file = parser.parse(file.path()).unwrap();

        assert_eq!(build_file.path, file.path().to_str().unwrap());
        // Note: Currently we don't collect target data,
        // but the parse should succeed
    }

    #[test]
    fn test_parse_content() {
        let parser = BuildFileParser::new();
        let content = indoc! {r#"
            package(name="test")
            target(name="target1", deps=[])
        "#};

        let result = parser.parse_content("BUILD", content);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_multiple_targets() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
target(name="target1", deps=[])
target(name="target2", deps=["target1"])
"#
        )
        .unwrap();
        file.flush().unwrap();

        let parser = BuildFileParser::new();
        let build_file = parser.parse(file.path()).unwrap();

        assert_eq!(build_file.path, file.path().to_str().unwrap());
    }

    #[test]
    fn test_parse_with_glob() {
        let parser = BuildFileParser::new();
        let content = indoc! {r#"
            srcs = glob("*.rs")
            target(name="lib", srcs=srcs)
        "#};

        let result = parser.parse_content("BUILD", content);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_complex_build() {
        let parser = BuildFileParser::new();
        let content = indoc! {r#"
            package(name="myapp")

            common_deps = [
                "//lib:common",
                "//lib:utils",
            ]

            target(
                name="server",
                srcs=glob("src/**/*.rs"),
                deps=common_deps + [":client"],
            )

            target(
                name="client",
                srcs=glob("client/**/*.rs"),
                deps=common_deps,
            )
        "#};

        let result = parser.parse_content("BUILD", content);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_error_handling() {
        let parser = BuildFileParser::new();
        let content = "invalid syntax )";

        let result = parser.parse_content("BUILD", content);
        assert!(result.is_err());
    }
}
