use crate::plugingo::driver_compile::{CompileParams, build_compile_spec};
use crate::plugingo::factors::Factors;
use crate::plugingo::target_std::archive_filename;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;

/// Build the first-party library compile (`go_compile`).
///
/// `golist_addr` is `Some` iff the package embeds — the driver reads its
/// `package.bin` for `EmbedPatterns`/`EmbedFiles` and resolves the embedcfg
/// in-process (no separate `go_embed` target). `embed_file_addrs` stage Go's
/// resolved `go_src` embeds; `embed_src_addrs` stage the `go_embed_src` lane.
#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
pub fn build_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: &[String],
    go_version: &str,
    golist_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    embed_src_addrs: &[String],
) -> TargetSpec {
    let out_file = archive_filename(import_path);
    // Go requires the main package to be compiled with -p "main" so the linker
    // can find main.main. Other packages use their full import path.
    let p_flag = if package_name == "main" {
        "main".to_string()
    } else {
        import_path.to_string()
    };
    build_compile_spec(CompileParams {
        addr,
        p_flag,
        out_file,
        factors,
        go_version,
        transitive_libs,
        src_addrs,
        s_files: &[],
        s_file_addrs: &[],
        hdr_addrs: &[],
        golist_addr,
        embed_variant: if golist_addr.is_some() { "embed" } else { "" },
        embed_file_addrs,
        embed_src_addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugingo::driver_compile::test_support::{cfg_list, cfg_str, deps_map};
    use hbuiltins::pluginfs;
    use hmodel::htpkg::PkgBuf;

    const V: &str = crate::plugingo::toolchain::DEFAULT_GO_VERSION;

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "build_lib".to_string(),
            Default::default(),
        )
    }
    fn golist_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "_golist".to_string(),
            Default::default(),
        )
    }
    fn src_addrs() -> Vec<String> {
        vec![
            pluginfs::file_addr("mylib/foo.go").format(),
            pluginfs::file_addr("mylib/bar.go").format(),
        ]
    }
    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }
    fn lib_spec(
        transitive: &[(String, Addr)],
        golist: Option<&Addr>,
        embed_files: &[String],
        embed_src: &[String],
        pkgname: &str,
    ) -> TargetSpec {
        build_spec(
            test_addr(),
            "example.com/mylib",
            pkgname,
            &test_factors(),
            transitive,
            &src_addrs(),
            V,
            golist,
            embed_files,
            embed_src,
        )
    }

    #[test]
    fn driver_is_go_compile() {
        assert_eq!(lib_spec(&[], None, &[], &[], "mylib").driver, "go_compile");
    }

    #[test]
    fn out_has_a_group() {
        let s = lib_spec(&[], None, &[], &[], "mylib");
        assert!(
            matches!(s.config.get("out").unwrap(), hcore::htvalue::Value::Map(m) if m.contains_key("a"))
        );
    }

    #[test]
    fn default_group_has_src_addrs() {
        let s = lib_spec(&[], None, &[], &[], "mylib");
        assert_eq!(deps_map(&s).get("").unwrap(), &src_addrs());
    }

    #[test]
    fn includes_hermetic_sdk() {
        let s = lib_spec(&[], None, &[], &[], "mylib");
        assert!(deps_map(&s).contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP));
    }

    #[test]
    fn main_uses_p_main() {
        assert_eq!(
            cfg_str(&lib_spec(&[], None, &[], &[], "main"), "p_flag"),
            "main"
        );
    }

    #[test]
    fn nonmain_uses_import_path() {
        assert_eq!(
            cfg_str(&lib_spec(&[], None, &[], &[], "mylib"), "p_flag"),
            "example.com/mylib"
        );
    }

    #[test]
    fn import_paths_and_lib_group_for_transitive() {
        let dep = (
            "fmt".to_string(),
            Addr::new(
                PkgBuf::from("@heph/go/std/fmt"),
                "build_lib".to_string(),
                Default::default(),
            ),
        );
        let s = lib_spec(std::slice::from_ref(&dep), None, &[], &[], "mylib");
        assert!(cfg_list(&s, "import_paths").contains(&"fmt".to_string()));
        assert!(deps_map(&s).contains_key("lib_fmt"));
    }

    #[test]
    fn embed_variant_and_golist_dep_when_embedding() {
        let g = golist_addr();
        let s = lib_spec(&[], Some(&g), &[], &[], "mylib");
        assert_eq!(cfg_list(&s, "embed_variant"), vec!["embed".to_string()]);
        assert!(deps_map(&s).contains_key("golist"));
    }

    #[test]
    fn no_embed_variant_without_golist() {
        let s = lib_spec(&[], None, &[], &[], "mylib");
        assert!(cfg_list(&s, "embed_variant").is_empty());
        assert!(!deps_map(&s).contains_key("golist"));
    }

    #[test]
    fn embed_src_group_present_when_addrs_given() {
        let g = golist_addr();
        let s = lib_spec(
            &[],
            Some(&g),
            &[],
            &["//mylib:frontend".to_string()],
            "mylib",
        );
        assert!(deps_map(&s).contains_key("embed_src"));
    }

    #[test]
    fn no_embed_src_group_when_absent() {
        let s = lib_spec(&[], None, &[], &[], "mylib");
        assert!(!deps_map(&s).contains_key("embed_src"));
    }

    #[test]
    fn s_files_empty_for_firstparty() {
        assert!(cfg_list(&lib_spec(&[], None, &[], &[], "mylib"), "s_files").is_empty());
    }

    #[test]
    fn has_go_build_label() {
        assert!(
            lib_spec(&[], None, &[], &[], "mylib")
                .labels
                .contains(&"go-build".to_string())
        );
    }
}
