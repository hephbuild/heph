//! Target reference parsing
//!
//! Parses target references like:
//! - `//pkg:target`
//! - `//pkg/subpkg:target`
//! - `:local_target`

use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::char,
    combinator::{map, opt},
    sequence::{preceded, tuple},
    IResult,
};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ParseError {
    #[error("Invalid target reference: {0}")]
    InvalidFormat(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRef {
    /// Package path (e.g., "//pkg/subpkg" or "" for local)
    pub package: String,
    /// Target name (e.g., "target")
    pub target: String,
}

impl TargetRef {
    /// Parse a target reference string
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        parse_target_ref(input)
            .map(|(_, tref)| tref)
            .map_err(|e| ParseError::InvalidFormat(e.to_string()))
    }

    /// Parse a target reference string in the context of a package
    /// Converts relative references (`:target`) to absolute (`//pkg:target`)
    pub fn parse_in_package(input: &str, pkg: &str) -> Result<Self, ParseError> {
        if input.starts_with(':') {
            // Relative reference - prepend package
            let full_ref = format!("//{}{}", pkg, input);
            Self::parse(&full_ref)
        } else {
            Self::parse(input)
        }
    }

    /// Format as string
    pub fn format(&self) -> String {
        if self.package.is_empty() {
            format!(":{}", self.target)
        } else {
            format!("{}:{}", self.package, self.target)
        }
    }

    /// Check if this is a relative reference (package is empty)
    pub fn is_relative(&self) -> bool {
        self.package.is_empty()
    }
}

// Parser implementation
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '@'
}

fn parse_package(input: &str) -> IResult<&str, String> {
    use nom::bytes::complete::take_while;

    map(
        preceded(
            tag("//"),
            take_while(|c: char| is_ident_char(c) || c == '/'),
        ),
        |s: &str| {
            if s.is_empty() {
                "//".to_string()
            } else {
                format!("//{}", s)
            }
        },
    )(input)
}

fn parse_local_ref(input: &str) -> IResult<&str, String> {
    map(char(':'), |_| String::new())(input)
}

fn parse_target_name(input: &str) -> IResult<&str, String> {
    map(take_while1(is_ident_char), |s: &str| s.to_string())(input)
}

fn parse_target_ref(input: &str) -> IResult<&str, TargetRef> {
    map(
        tuple((
            alt((parse_package, parse_local_ref)),
            preceded(opt(char(':')), parse_target_name),
        )),
        |(package, target)| TargetRef { package, target },
    )(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_ref() {
        let tref = TargetRef::parse("//pkg/subpkg:target").unwrap();
        assert_eq!(tref.package, "//pkg/subpkg");
        assert_eq!(tref.target, "target");
    }

    #[test]
    fn test_parse_simple_ref() {
        let tref = TargetRef::parse("//pkg:target").unwrap();
        assert_eq!(tref.package, "//pkg");
        assert_eq!(tref.target, "target");
    }

    #[test]
    fn test_parse_local_ref() {
        let tref = TargetRef::parse(":local").unwrap();
        assert_eq!(tref.package, "");
        assert_eq!(tref.target, "local");
        assert!(tref.is_relative());
    }

    #[test]
    fn test_parse_root_ref() {
        let tref = TargetRef::parse("//:name").unwrap();
        assert_eq!(tref.package, "//");
        assert_eq!(tref.target, "name");
    }

    #[test]
    fn test_parse_in_package() {
        let tref = TargetRef::parse_in_package(":local", "some/pkg").unwrap();
        assert_eq!(tref.package, "//some/pkg");
        assert_eq!(tref.target, "local");
    }

    #[test]
    fn test_format() {
        let tref = TargetRef {
            package: "//pkg".to_string(),
            target: "tgt".to_string(),
        };
        assert_eq!(tref.format(), "//pkg:tgt");
    }

    #[test]
    fn test_format_local() {
        let tref = TargetRef {
            package: String::new(),
            target: "local".to_string(),
        };
        assert_eq!(tref.format(), ":local");
    }

    #[test]
    fn test_parse_invalid() {
        assert!(TargetRef::parse("invalid").is_err());
    }

    #[test]
    fn test_roundtrip() {
        let inputs = vec![
            "//foo/bar:baz",
            "//pkg:target",
            "//:root",
            ":local",
        ];

        for input in inputs {
            let tref = TargetRef::parse(input).unwrap();
            assert_eq!(tref.format(), input, "Failed roundtrip for {}", input);
        }
    }
}
