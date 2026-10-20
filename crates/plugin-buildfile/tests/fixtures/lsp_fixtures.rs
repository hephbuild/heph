// Every editor-shaped fixture the sweeps run over, cursor marked with `│`.
// Shared by `tests/lsp.rs` (the variant and text-edit sweeps) and the lexer's
// unit tests (the prefix sweep) through `include!`, so "every fixture" means
// the same list in both. The workspace the harness opens them in has
// `lib/BUILD` (target `util`) and `x/y.star` (defs `foo`, `fun`).
//
// `(name, fixture, excluded from variant (c))`: an open `"""` swallows the
// rest of the file, so a line inserted above it is not a neutral edit.
const FIXTURES: &[(&str, &str, bool)] = &[
    // Per site kind.
    ("callee", "tar│get(name = \"x\", driver = \"exec\")\n", false),
    ("driver_value", "target(name = \"x\", driver = \"ex│\")\n", false),
    ("driver_value_single", "target(name = 'x', driver = 'ex│')\n", false),
    ("provider_value", "provider_state(provider = \"│\")\n", false),
    ("target_key", "target(name = \"x\", driver = \"exec\", │)\n", false),
    ("target_key_prefix", "target(name = \"x\", dri│)\n", false),
    ("state_key", "provider_state(provider = \"go\", │)\n", false),
    ("plugin_namespace", "x = heph.│\n", false),
    ("plugin_member", "x = heph.fs.│\n", false),
    ("address", "target(name = \"x\", deps = [\"//lib:│\"])\n", false),
    ("address_relative", "target(name = \"x\", deps = [\":│\"])\n", false),
    ("load_path", "load(\"//│\", \"make_name\")\n", false),
    ("load_symbol", "load(\"//x:y.star\", \"f│\")\n", false),
    ("none_value", "MY_CMD = \"x\"\ntarget(name = \"x\", cmd = M│)\n", false),
    ("arg_key", "def helper(alpha, beta = 1):\n    pass\n\nhelper(alpha = 1, │)\n", false),
    ("arg_key_prefix", "def helper(alpha, beta = 1):\n    pass\n\nhelper(be│)\n", false),
    ("arg_key_unclosed", "def helper(alpha, beta = 1):\n    pass\n\nhelper(be│", false),
    // While typing.
    ("unclosed_at_eof", "target(│", false),
    ("unclosed_filters", "target(name = \"x\", dri│", false),
    ("missing_comma", "target(\n    name = \"x\"\n    dri│\n)\n", false),
    ("unterminated_driver", "target(name = \"x\", driver = \"ex│\n", false),
    ("swallowed_paren", "target(name = \"x\", driver = \"ex│)", false),
    ("unterminated_crlf", "target(name = \"x\", driver = \"ex│\r\n", false),
    ("lone_quote_eof", "target(name = \"x\", driver = \"│", false),
    ("raw_prefix", "target(name = \"x\", driver = r\"ex│\n", false),
    ("bytes_prefix", "target(name = \"x\", driver = b\"ex│\n", false),
    ("apostrophe", "target(driver = \"don't│\n", false),
    ("quote_before_string", "target(name = \"x\", driver = \"│\"exec\"\n", false),
    ("invalid_escape", "target(driver = \"ex\\xZZ\", na│\n", false),
    ("unterminated_fstring", "target(driver = f\"ex│\n    name = \"x\",\n", false),
    ("fstring_expression_above", "x = f\"abc{y\ntarget(name = \"x\", dri│)\n", false),
    ("error_line_above", "x = \"oops\ntarget(dri│\n", false),
    ("stray_char", "target(name = $, dri│\n", false),
    ("bad_dedent_above", "if x:\n        a = 1\n    b = 2\ntarget(name = \"x\", dri│)\n", false),
    ("tab_body", "def f():\n\ttarget(name = \"x\", dri│\n", false),
    ("open_triple_above", "\"\"\"abc\ntarget(dri│\n", true),
    ("open_triple_below", "target(name = \"x\", dri│)\n\"\"\"abc\n", true),
    ("mismatch_before", "foo(]\ntarget(dri│\n", false),
    ("mismatch_after", "deps = [ target(│ ], name = \"y\")\n", false),
    ("later_statement", "target(dri│)\ntarget(name = \"y\", driver = \"ex\n", false),
    ("column_zero_ends", "target(dri│\nname = \"y\"\n", false),
    ("def_indent_ends", "def m():\n    target(dri│\n    other(name = \"y\")\n", false),
    ("triple_body_column_zero", "target(\n    │\n    labels = \"\"\"\nfoo\n\"\"\",\n", false),
    ("comment_column_zero", "target(\n    │\n# note\n    labels = [\"a\"],\n", false),
    ("earlier_unclosed", "target(\nx = │\n", false),
    ("namespace_broken_tail", "x = heph.│\ny = (\n", false),
    ("value_no_string", "target(name = \"x\", driver = │)\n", false),
    ("value_ident", "target(name = \"x\", driver = ex│)\n", false),
    ("def_second_call", "def m():\n    target(dri│\n    target(name = \"y\", driver = \"exec\")\n", false),
];
