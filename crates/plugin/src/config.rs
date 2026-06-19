//! Plugin option parsing: the small, engine-free slice of config handling that
//! every provider/driver factory uses to read its `options:` map. The types now
//! live in the foundational `config` crate (`hconfig`); re-exported here so
//! `hplugin::config::{Options, decode_opt, deny_unknown}` keeps resolving for
//! plugin authors.

pub use hconfig::{Options, decode_opt, deny_unknown};
