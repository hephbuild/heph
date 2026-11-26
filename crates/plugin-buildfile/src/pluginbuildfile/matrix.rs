//! `heph.core.matrix()`: enumerate the cells of a matrix, call a cell function
//! once per cell, and return what each call returned keyed by cell, plus the
//! value of a default cell. See `docs/MATRIX.md`.
//!
//! The builtin declares nothing and never reads what a cell returns (I1, I3):
//! a target a cell declares is exactly what the cell called, as if the BUILD
//! file had written the same calls on that line. Keys and cell order are a pure
//! function of `axes`, `exclude` and `include` (I4); only *which* cell
//! `.default` reads depends on the host, and a host with no cell raises only
//! when `.default` is read (I8).
//!
//! The planning half (validation, the cross product, `exclude`/`include`, the
//! default) is plain Rust over strings, so it is tested without an evaluator;
//! [`matrix`] is the Starlark half.

use crate::pluginbuildfile::run_file::{Extra, Host, rel_display};
use allocative::Allocative;
use either::Either;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef, DictType};
use starlark::values::list::{ListRef, ListType};
use starlark::values::none::NoneOr;
use starlark::values::tuple::AllocTuple;
use starlark::values::typing::{StarlarkCallable, StarlarkCallableParamAny};
use starlark::values::{
    AllocValue, Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue,
    Trace, UnpackValue, Value, ValueLike, starlark_value,
};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// The most cells one call may have, counted as the cross product (before
/// `exclude`) plus `include`, before the first cell runs (I10). A constant, not
/// configuration: real matrices are 3 to about 120 cells, and raising it later
/// breaks nothing.
pub(crate) const MAX_CELLS: u64 = 1024;

/// How many cells the "no cell for this host" error lists before eliding.
const LISTED_CELLS: usize = 16;

/// Starlark keywords: an axis named like one could not be a parameter of the
/// cell function.
const KEYWORDS: &[&str] = &[
    "and", "as", "assert", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "load", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

/// Where a call is, for its errors: `p/BUILD:12`.
#[derive(Debug, Clone)]
pub(crate) struct Site(pub String);

impl Site {
    fn err(&self, msg: impl fmt::Display) -> anyhow::Error {
        anyhow::anyhow!("heph.core.matrix() at {}: {msg}", self.0)
    }
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The validated axes, in declaration order: that order orders tuple keys.
#[derive(Debug)]
pub(crate) struct Axes {
    names: Vec<String>,
    values: Vec<Vec<String>>,
}

/// One cell: a value per axis, in axis order.
pub(crate) type Cell = Vec<String>;

/// An `exclude` or `include` entry as written: axis to value, in dict order.
pub(crate) type Entry = Vec<(String, String)>;

impl Axes {
    pub(crate) fn new(site: &Site, entries: Vec<(String, Vec<String>)>) -> anyhow::Result<Self> {
        if entries.is_empty() {
            return Err(site.err("`axes` is empty; give at least one axis"));
        }
        let mut names = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());
        for (name, vals) in entries {
            if !is_identifier(&name) {
                return Err(site.err(format_args!(
                    "axis {name:?} is not a Starlark identifier; it becomes a keyword argument of \
                     the cell function (use `_`, not `-`)"
                )));
            }
            if vals.is_empty() {
                return Err(site.err(format_args!("axis {name:?} has no values")));
            }
            for (i, v) in vals.iter().enumerate() {
                validate_value(site, &name, v)?;
                if vals.get(..i).is_some_and(|before| before.contains(v)) {
                    return Err(site.err(format_args!("axis {name:?} lists {v:?} twice")));
                }
            }
            names.push(name);
            values.push(vals);
        }
        Ok(Self { names, values })
    }

    fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    fn known(&self) -> String {
        self.names.join(", ")
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&s)
}

/// A value is `[A-Za-z0-9._/+-]+`, ASCII only: it ends up in target names
/// (D8) and in error texts.
fn validate_value(site: &Site, axis: &str, v: &str) -> anyhow::Result<()> {
    let ok = !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'+' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(site.err(format_args!(
            "axis {axis:?} value {v:?} is not allowed; a value is one or more of A-Z a-z 0-9 . _ / + -"
        )))
    }
}

/// `axis=value, axis=value`: how errors and logs name a cell.
pub(crate) fn describe(names: &[String], cell: &[String]) -> String {
    let mut out = String::new();
    for (i, (n, v)) in names.iter().zip(cell).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(n);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// `2048` as `2,048`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The cells of the matrix, in cell order: the cross product (last axis
/// fastest) minus `exclude`, then `include` in list order (D15). Checks the
/// cell limit before enumerating anything (I10).
pub(crate) fn plan(
    site: &Site,
    axes: &Axes,
    exclude: &[Entry],
    include: &[Entry],
) -> anyhow::Result<Vec<Cell>> {
    // Multiplied with overflow checks, before `exclude`: the count a run would
    // pay for, not what is left after it.
    let product = axes
        .values
        .iter()
        .try_fold(1u64, |acc, v| acc.checked_mul(v.len() as u64));
    let total = product.and_then(|p| p.checked_add(include.len() as u64));
    match total {
        Some(n) if n <= MAX_CELLS => {}
        Some(n) => {
            return Err(anyhow::anyhow!(
                "heph.core.matrix() at {site} would have {} cells; the limit is {}",
                thousands(n),
                thousands(MAX_CELLS),
            ));
        }
        None => {
            return Err(anyhow::anyhow!(
                "heph.core.matrix() at {site} would have more than {} cells; the limit is {}",
                thousands(u64::MAX),
                thousands(MAX_CELLS),
            ));
        }
    }

    // An exclude may name a subset of the axes, only axes that exist, and must
    // match a cell of the full cross product: a value outside its axis is a
    // stale exclude, which is a typo.
    let mut excludes: Vec<Vec<(usize, &str)>> = Vec::with_capacity(exclude.len());
    for e in exclude {
        let mut m = Vec::with_capacity(e.len());
        for (axis, value) in e {
            let Some(i) = axes.index(axis) else {
                return Err(site.err(format_args!(
                    "exclude names axis {axis:?}, which is not an axis of this matrix ({})",
                    axes.known()
                )));
            };
            if !axes.values.get(i).is_some_and(|vs| vs.contains(value)) {
                return Err(site.err(format_args!(
                    "exclude {} matches no cell: {value:?} is not a value of axis {axis:?}",
                    describe_entry(e)
                )));
            }
            m.push((i, value.as_str()));
        }
        excludes.push(m);
    }

    let mut cells: Vec<Cell> = Vec::new();
    let mut at = vec![0usize; axes.values.len()];
    'product: loop {
        let excluded = excludes.iter().any(|m| {
            m.iter().all(|&(i, v)| {
                at.get(i)
                    .and_then(|&j| axes.values.get(i).and_then(|vs| vs.get(j)))
                    .is_some_and(|x| x == v)
            })
        });
        if !excluded {
            cells.push(
                at.iter()
                    .zip(&axes.values)
                    .filter_map(|(&j, vs)| vs.get(j).cloned())
                    .collect(),
            );
        }
        // Odometer step, last axis fastest; done when the first axis wraps.
        for (slot, vs) in at.iter_mut().zip(&axes.values).rev() {
            *slot += 1;
            if *slot < vs.len() {
                continue 'product;
            }
            *slot = 0;
        }
        break;
    }

    // An include gives every axis and nothing else; its value may be outside
    // the axis list. It may add back an excluded cell, never one still there.
    for e in include {
        let mut cell: Vec<Option<String>> = vec![None; axes.names.len()];
        for (axis, value) in e {
            let Some(i) = axes.index(axis) else {
                return Err(site.err(format_args!(
                    "include {} names axis {axis:?}, which is not an axis of this matrix ({})",
                    describe_entry(e),
                    axes.known()
                )));
            };
            validate_value(site, axis, value)?;
            if let Some(slot) = cell.get_mut(i) {
                *slot = Some(value.clone());
            }
        }
        let missing: Vec<&str> = axes
            .names
            .iter()
            .zip(&cell)
            .filter(|(_, v)| v.is_none())
            .map(|(n, _)| n.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(site.err(format_args!(
                "include {} gives no value for axis {}; an include names every axis",
                describe_entry(e),
                missing.join(", ")
            )));
        }
        let cell: Cell = cell.into_iter().flatten().collect();
        if cells.contains(&cell) {
            return Err(site.err(format_args!(
                "include {} is already a cell of this matrix",
                describe(&axes.names, &cell)
            )));
        }
        cells.push(cell);
    }
    Ok(cells)
}

fn describe_entry(e: &Entry) -> String {
    let names: Vec<String> = e.iter().map(|(n, _)| n.clone()).collect();
    let values: Vec<String> = e.iter().map(|(_, v)| v.clone()).collect();
    format!("{{{}}}", describe(&names, &values))
}

/// The `default` argument, once unpacked.
#[derive(Debug)]
pub(crate) enum DefaultArg {
    /// `default = "host"`.
    Host,
    /// `default = {axis: value}`; `"host"` stands for the host's platform.
    Dict(Entry),
}

/// Which cell `.default` reads, worked out at the call.
#[derive(Debug, PartialEq)]
pub(crate) enum DefaultCell {
    /// The index into the plan's cells.
    Index(usize),
    /// The host has no cell: the error `.default` raises when read.
    NoHostCell(String),
}

/// The value `"host"` stands for on `axis`, if it stands for one there.
fn host_value(axis: &str, host: Host) -> Option<String> {
    match axis {
        "platform" => Some(host.platform()),
        "os" => Some(host.os.to_string()),
        "arch" => Some(host.arch.to_string()),
        _ => None,
    }
}

/// Resolve `default` against the planned cells. Structural mistakes (an
/// unknown axis, a missing one, a dict naming no cell, `"host"` on axes that
/// cannot take it) fail here; only a host with no cell waits for the read.
pub(crate) fn resolve_default(
    site: &Site,
    axes: &Axes,
    cells: &[Cell],
    arg: &DefaultArg,
    host: Host,
) -> anyhow::Result<DefaultCell> {
    let entry: Entry = match arg {
        DefaultArg::Host => {
            let names: Vec<&str> = axes.names.iter().map(String::as_str).collect();
            match names.as_slice() {
                ["platform"] => vec![("platform".into(), "host".into())],
                ["os", "arch"] | ["arch", "os"] => {
                    vec![("os".into(), "host".into()), ("arch".into(), "host".into())]
                }
                _ => {
                    let example: Vec<String> = axes
                        .names
                        .iter()
                        .map(|n| match n.as_str() {
                            "platform" | "os" | "arch" => format!("{n:?}: \"host\""),
                            _ => format!("{n:?}: \"<value>\""),
                        })
                        .collect();
                    return Err(site.err(format_args!(
                        "default = \"host\" needs one axis named platform, or two named os and \
                         arch; this matrix has axes {}. Pass a dict instead: default = {{{}}}",
                        axes.known(),
                        example.join(", ")
                    )));
                }
            }
        }
        DefaultArg::Dict(e) => e.clone(),
    };

    let mut fixed: Vec<(usize, String)> = Vec::new();
    let mut wanted: Vec<Option<String>> = vec![None; axes.names.len()];
    for (axis, value) in &entry {
        let Some(i) = axes.index(axis) else {
            return Err(site.err(format_args!(
                "default names axis {axis:?}, which is not an axis of this matrix ({})",
                axes.known()
            )));
        };
        let v = if value == "host" {
            let Some(hv) = host_value(axis, host) else {
                return Err(site.err(format_args!(
                    "default {axis:?}: \"host\" stands for the host's platform; only an axis \
                     named platform, os or arch takes it"
                )));
            };
            if axis == "platform"
                && let Some(bad) = cells
                    .iter()
                    .filter_map(|c| c.get(i))
                    .find(|v| !v.contains('/'))
            {
                return Err(site.err(format_args!(
                    "default = \"host\" matches platform values as os/arch, and {bad:?} has no `/`"
                )));
            }
            hv
        } else {
            fixed.push((i, value.clone()));
            value.clone()
        };
        if let Some(slot) = wanted.get_mut(i) {
            *slot = Some(v);
        }
    }
    let missing: Vec<&str> = axes
        .names
        .iter()
        .zip(&wanted)
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| n.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(site.err(format_args!(
            "default gives no value for axis {}; a dict default names every axis",
            missing.join(", ")
        )));
    }
    // The fixed part must name a cell whatever the host: a typo there is the
    // user's on every host.
    if !cells
        .iter()
        .any(|c| fixed.iter().all(|(i, v)| c.get(*i) == Some(v)))
    {
        let names: Vec<String> = fixed
            .iter()
            .filter_map(|(i, _)| axes.names.get(*i).cloned())
            .collect();
        let values: Vec<String> = fixed.iter().map(|(_, v)| v.clone()).collect();
        return Err(site.err(format_args!(
            "default {{{}}} names no cell of this matrix",
            describe(&names, &values)
        )));
    }

    let wanted: Cell = wanted.into_iter().flatten().collect();
    if let Some(i) = cells.iter().position(|c| *c == wanted) {
        return Ok(DefaultCell::Index(i));
    }
    let mut listed: Vec<String> = cells
        .iter()
        .take(LISTED_CELLS)
        .map(|c| describe(&axes.names, c))
        .collect();
    if cells.len() > LISTED_CELLS {
        listed.push(format!("and {} more", cells.len() - LISTED_CELLS));
    }
    Ok(DefaultCell::NoHostCell(format!(
        "heph.core.matrix() at {site} has no cell for this host ({}). Its cells: {}. Add the host \
         with include, or pass default={{...}}.",
        describe(&axes.names, &wanted),
        listed.join(", ")
    )))
}

/// What `.default` reads, stored at the call. Never looked up in `cells`,
/// which BUILD code can mutate before freezing (D16).
#[derive(Debug, Allocative)]
enum DefaultState {
    /// `default = None`.
    Absent { site: Box<str> },
    /// The default cell's value is in [`MatrixGen::default`].
    Present {
        site: Box<str>,
        host: Box<str>,
        cell: Box<str>,
    },
    /// The host has no cell: the error a read raises.
    Missing { error: Box<str> },
}

/// The value `heph.core.matrix()` returns: `cells` and `default` are
/// attributes. Stores the default value, or the error, at the call.
#[derive(Debug, Trace, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct MatrixGen<V> {
    cells: V,
    default: Option<V>,
    state: DefaultState,
}

impl<V: fmt::Display> fmt::Display for MatrixGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "matrix(cells = {})", self.cells)
    }
}

impl<'v> Freeze for MatrixGen<Value<'v>> {
    type Frozen = MatrixGen<FrozenValue>;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(MatrixGen {
            cells: self.cells.freeze(freezer)?,
            default: self.default.map(|v| v.freeze(freezer)).transpose()?,
            state: self.state,
        })
    }
}

impl<'v> AllocValue<'v> for MatrixGen<Value<'v>> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "matrix")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for MatrixGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new("heph.core.matrix", matrix_methods);
        Some(RES.methods())
    }
}

/// The fields of a matrix value, whether it is frozen or not.
fn view<'v>(this: Value<'v>) -> starlark::Result<(Value<'v>, Option<Value<'v>>, &'v DefaultState)> {
    if let Some(m) = this.downcast_ref::<MatrixGen<Value<'v>>>() {
        return Ok((m.cells, m.default, &m.state));
    }
    if let Some(m) = this
        .unpack_frozen()
        .and_then(|f| f.downcast_ref::<MatrixGen<FrozenValue>>())
    {
        return Ok((
            m.cells.to_value(),
            m.default.map(|v| v.to_value()),
            &m.state,
        ));
    }
    Err(starlark::Error::new_other(anyhow::anyhow!(
        "expected a matrix, got {}",
        this.get_type()
    )))
}

#[starlark_module]
fn matrix_methods(builder: &mut MethodsBuilder) {
    /// What each cell's function returned, keyed by cell, in cell order: the
    /// bare value for one axis, a tuple in axis order for several.
    #[starlark(attribute)]
    fn cells<'v>(this: Value<'v>) -> starlark::Result<Value<'v>> {
        Ok(view(this)?.0)
    }

    /// The default cell's value. Raises when the host has no cell, or when the
    /// matrix was made with `default = None`.
    #[starlark(attribute)]
    fn default<'v>(this: Value<'v>) -> starlark::Result<Value<'v>> {
        let (_, value, state) = view(this)?;
        match (state, value) {
            (DefaultState::Present { site, host, cell }, Some(v)) => {
                tracing::debug!(
                    site = &**site,
                    host = &**host,
                    cell = &**cell,
                    "matrix default read"
                );
                Ok(v)
            }
            (DefaultState::Missing { error }, _) => {
                Err(starlark::Error::new_other(anyhow::anyhow!("{error}")))
            }
            (DefaultState::Absent { site }, _) => Err(starlark::Error::new_other(anyhow::anyhow!(
                "heph.core.matrix() at {site} has no default: it was made without `default`. \
                 Pass default = \"host\" or a dict of axis values to pick one"
            ))),
            (DefaultState::Present { site, .. }, None) => Err(starlark::Error::new_other(
                anyhow::anyhow!("heph.core.matrix() at {site}: default value missing"),
            )),
        }
    }
}

/// Resets [`Extra::cell`] when a cell's call ends, error or not.
struct CellGuard<'a>(&'a Extra<'a>);

impl Drop for CellGuard<'_> {
    fn drop(&mut self) {
        *self.0.cell.borrow_mut() = None;
    }
}

/// The call site of the builtin being run: `p/BUILD:12`.
pub(crate) fn call_site(eval: &Evaluator, root: &Path) -> Site {
    match eval.call_stack_top_location() {
        Some(span) => Site(format!(
            "{}:{}",
            rel_display(root, Path::new(span.filename())),
            span.resolve_span().begin.line + 1
        )),
        None => Site("<unknown>".to_string()),
    }
}

fn type_err(site: &Site, what: &str, v: Value) -> starlark::Error {
    starlark::Error::new_other(site.err(format_args!("{what}, got {}", v.get_type())))
}

/// A list of strings, refusing anything else (a string is not iterated).
fn unpack_strs<'v>(site: &Site, what: &str, v: Value<'v>) -> starlark::Result<Vec<String>> {
    let Some(list) = ListRef::from_value(v) else {
        return Err(type_err(
            site,
            &format!("{what} must be a list of strings"),
            v,
        ));
    };
    list.content()
        .iter()
        .map(|e| {
            e.unpack_str()
                .map(str::to_string)
                .ok_or_else(|| type_err(site, &format!("{what} must hold strings"), *e))
        })
        .collect()
}

/// A dict of string to string, in dict order.
fn unpack_entry<'v>(site: &Site, what: &str, v: Value<'v>) -> starlark::Result<Entry> {
    let Some(d) = DictRef::from_value(v) else {
        return Err(type_err(
            site,
            &format!("{what} must be a dict of axis to value"),
            v,
        ));
    };
    d.iter()
        .map(|(k, val)| {
            let (Some(k), Some(val)) = (k.unpack_str(), val.unpack_str()) else {
                return Err(type_err(
                    site,
                    &format!("{what} must map axis names to string values"),
                    if k.unpack_str().is_none() { k } else { val },
                ));
            };
            Ok((k.to_string(), val.to_string()))
        })
        .collect()
}

fn unpack_entries<'v>(
    site: &Site,
    what: &str,
    v: Option<Value<'v>>,
) -> starlark::Result<Vec<Entry>> {
    let Some(v) = v.filter(|v| !v.is_none()) else {
        return Ok(vec![]);
    };
    let Some(list) = ListRef::from_value(v) else {
        return Err(type_err(
            site,
            &format!("`{what}` must be a list of dicts"),
            v,
        ));
    };
    list.content()
        .iter()
        .map(|e| unpack_entry(site, &format!("each `{what}` entry"), *e))
        .collect()
}

// The parameter types the lint and the LSP see. Unchecked at run time: the
// builtin refuses a wrong shape itself, with an error naming the call.
/// `dict[str, list[str]]`.
pub(crate) type AxesTy = DictType<String, ListType<String>>;
/// `None | typing.Callable`.
pub(crate) type CellTy<'v> = NoneOr<StarlarkCallable<'v>>;
/// `None | str | dict[str, str]`.
pub(crate) type DefaultTy = NoneOr<Either<String, DictType<String, String>>>;
/// `None | list[dict[str, str]]`.
pub(crate) type EntriesTy = NoneOr<ListType<DictType<String, String>>>;

/// The arguments of one `heph.core.matrix()` call.
pub(crate) struct Args<'v> {
    pub axes: Value<'v>,
    pub cell: Option<Value<'v>>,
    pub default: Option<Value<'v>>,
    pub exclude: Option<Value<'v>>,
    pub include: Option<Value<'v>>,
}

/// `heph.core.matrix()`.
pub(crate) fn matrix<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    extra: &Extra,
    args: Args<'v>,
) -> starlark::Result<Value<'v>> {
    let site = call_site(eval, extra.root);
    if extra.cell.borrow().is_some() {
        return Err(starlark::Error::new_other(
            site.err("heph.core.matrix() inside a cell"),
        ));
    }

    let Some(axes_dict) = DictRef::from_value(args.axes) else {
        return Err(type_err(
            &site,
            "`axes` must be a dict of axis name to list of values",
            args.axes,
        ));
    };
    let entries = axes_dict
        .iter()
        .map(|(k, v)| {
            let Some(name) = k.unpack_str() else {
                return Err(type_err(&site, "an axis name must be a string", k));
            };
            Ok((
                name.to_string(),
                unpack_strs(&site, &format!("axis {name:?}"), v)?,
            ))
        })
        .collect::<starlark::Result<Vec<_>>>()?;
    drop(axes_dict);
    let axes = Axes::new(&site, entries).map_err(starlark::Error::new_other)?;
    let exclude = unpack_entries(&site, "exclude", args.exclude)?;
    let include = unpack_entries(&site, "include", args.include)?;
    let cells = plan(&site, &axes, &exclude, &include).map_err(starlark::Error::new_other)?;

    let default_arg = match args.default.filter(|v| !v.is_none()) {
        None => None,
        Some(v) if v.unpack_str() == Some("host") => Some(DefaultArg::Host),
        Some(v) if DictRef::from_value(v).is_some() => {
            Some(DefaultArg::Dict(unpack_entry(&site, "`default`", v)?))
        }
        Some(v) => {
            return Err(type_err(
                &site,
                "`default` must be None, \"host\" or a dict of axis to value",
                v,
            ));
        }
    };
    let default_cell = default_arg
        .as_ref()
        .map(|a| resolve_default(&site, &axes, &cells, a, extra.host))
        .transpose()
        .map_err(starlark::Error::new_other)?;

    let cell_fn = match args.cell.filter(|v| !v.is_none()) {
        None => None,
        Some(f)
            if StarlarkCallable::<'v, StarlarkCallableParamAny>::unpack_value_opt(f).is_some() =>
        {
            Some(f)
        }
        Some(f) => return Err(type_err(&site, "`cell` must be callable", f)),
    };

    let heap = eval.heap();
    let mut pairs: Vec<(Value<'v>, Value<'v>)> = Vec::with_capacity(cells.len());
    let mut default_value = None;
    for (i, cell) in cells.iter().enumerate() {
        let key = match cell.as_slice() {
            [one] => heap.alloc(one.as_str()),
            many => heap.alloc(AllocTuple(many.iter().map(String::as_str))),
        };
        let value = match cell_fn {
            None => heap.alloc(AllocDict(
                axes.names
                    .iter()
                    .map(String::as_str)
                    .zip(cell.iter().map(String::as_str)),
            )),
            Some(f) => {
                let desc = describe(&axes.names, cell);
                *extra.cell.borrow_mut() = Some(Arc::from(desc.as_str()));
                let _guard = CellGuard(extra);
                let named: Vec<(&str, Value<'v>)> = axes
                    .names
                    .iter()
                    .map(String::as_str)
                    .zip(cell.iter().map(|v| heap.alloc(v.as_str())))
                    .collect();
                eval.eval_function(f, &[], &named).map_err(|e| {
                    e.with_context(format!("heph.core.matrix() at {site}: cell {desc}"))
                })?
            }
        };
        if default_cell == Some(DefaultCell::Index(i)) {
            default_value = Some(value);
        }
        pairs.push((key, value));
    }

    let state = match default_cell {
        None => DefaultState::Absent {
            site: site.0.into(),
        },
        Some(DefaultCell::Index(i)) => DefaultState::Present {
            site: site.0.into(),
            host: extra.host.platform().into(),
            cell: cells
                .get(i)
                .map(|c| describe(&axes.names, c))
                .unwrap_or_default()
                .into(),
        },
        Some(DefaultCell::NoHostCell(error)) => DefaultState::Missing {
            error: error.into(),
        },
    };
    Ok(heap.alloc(MatrixGen {
        cells: heap.alloc(AllocDict(pairs)),
        default: default_value,
        state,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> Site {
        Site("p/BUILD:12".to_string())
    }

    fn axes(entries: &[(&str, &[&str])]) -> anyhow::Result<Axes> {
        Axes::new(
            &site(),
            entries
                .iter()
                .map(|(n, vs)| (n.to_string(), vs.iter().map(|v| v.to_string()).collect()))
                .collect(),
        )
    }

    fn entry(pairs: &[(&str, &str)]) -> Entry {
        pairs
            .iter()
            .map(|(a, v)| (a.to_string(), v.to_string()))
            .collect()
    }

    fn cells_of(cells: &[Cell]) -> Vec<String> {
        cells.iter().map(|c| c.join(" ")).collect()
    }

    const PLATS: &[&str] = &["linux/amd64", "linux/arm64", "darwin/arm64"];

    fn host(os: &'static str, arch: &'static str) -> Host {
        Host {
            os,
            arch,
            os_raw: os,
            arch_raw: arch,
        }
    }

    #[test]
    fn plan_orders_last_axis_fastest_and_applies_d15() {
        let a = axes(&[("go", &["1.22", "1.23"]), ("race", &["on", "off"])]).unwrap();
        assert_eq!(
            cells_of(&plan(&site(), &a, &[], &[]).unwrap()),
            ["1.22 on", "1.22 off", "1.23 on", "1.23 off"]
        );
        // Overlapping excludes; an include re-adding an excluded cell; an
        // include with a value outside the axis list; includes in list order.
        let got = plan(
            &site(),
            &a,
            &[
                entry(&[("go", "1.22")]),
                entry(&[("go", "1.22"), ("race", "on")]),
            ],
            &[
                entry(&[("race", "on"), ("go", "1.22")]),
                entry(&[("go", "1.24rc1"), ("race", "off")]),
            ],
        )
        .unwrap();
        assert_eq!(
            cells_of(&got),
            ["1.23 on", "1.23 off", "1.22 on", "1.24rc1 off"]
        );
        // Excluding every cell is allowed.
        assert!(plan(&site(), &a, &[entry(&[])], &[]).unwrap().is_empty());
    }

    #[test]
    fn plan_refuses_bad_excludes_and_includes() {
        let a = axes(&[("go", &["1.22", "1.23"]), ("race", &["on", "off"])]).unwrap();
        let err =
            |ex: &[Entry], inc: &[Entry]| format!("{:#}", plan(&site(), &a, ex, inc).unwrap_err());
        let e = err(&[entry(&[("os", "linux")])], &[]);
        assert!(e.contains("exclude names axis \"os\""), "{e}");
        assert!(e.starts_with("heph.core.matrix() at p/BUILD:12: "), "{e}");
        let e = err(&[entry(&[("go", "1.21")])], &[]);
        assert!(e.contains("matches no cell"), "{e}");
        let e = err(&[], &[entry(&[("go", "1.22")])]);
        assert!(e.contains("gives no value for axis race"), "{e}");
        let e = err(
            &[],
            &[entry(&[("go", "1.22"), ("race", "on"), ("os", "x")])],
        );
        assert!(e.contains("names axis \"os\""), "{e}");
        let e = err(&[], &[entry(&[("go", "1.22"), ("race", "on")])]);
        assert!(e.contains("is already a cell"), "{e}");
        // Two equal includes: the second duplicates the first.
        let inc = entry(&[("go", "1.24"), ("race", "on")]);
        let e = err(&[], &[inc.clone(), inc]);
        assert!(e.contains("is already a cell"), "{e}");
        let e = err(&[], &[entry(&[("go", "1 24"), ("race", "on")])]);
        assert!(e.contains("is not allowed"), "{e}");
    }

    #[test]
    fn axes_refuse_bad_names_and_values() {
        let err = |entries: &[(&str, &[&str])]| format!("{:#}", axes(entries).unwrap_err());
        assert!(err(&[]).contains("`axes` is empty"));
        assert!(err(&[("go", &[])]).contains("has no values"));
        assert!(err(&[("go-version", &["1"])]).contains("not a Starlark identifier"));
        assert!(err(&[("if", &["1"])]).contains("not a Starlark identifier"));
        assert!(err(&[("1go", &["1"])]).contains("not a Starlark identifier"));
        assert!(err(&[("go", &["1", "1"])]).contains("lists \"1\" twice"));
        for bad in ["a b", "a@b", "a:b", "a|b", "a\nb", "é", "１", ""] {
            assert!(
                err(&[("v", &[bad])]).contains("is not allowed"),
                "{bad:?} was accepted"
            );
        }
        for ok in ["..", "/", "a//b", "1.2+x_y-Z"] {
            assert!(axes(&[("v", &[ok])]).is_ok(), "{ok:?} was refused");
        }
    }

    #[test]
    fn the_limit_is_checked_before_anything_is_enumerated() {
        let two: &[&str] = &["a", "b"];
        // 2^10 = 1,024 is the limit exactly.
        let ten: Vec<(String, &[&str])> = (0..10).map(|i| (format!("a{i}"), two)).collect();
        let ten: Vec<(&str, &[&str])> = ten.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let a = axes(&ten).unwrap();
        assert_eq!(plan(&site(), &a, &[], &[]).unwrap().len(), 1024);
        // One include more is over it.
        let inc = entry(&ten.iter().map(|(n, _)| (*n, "c")).collect::<Vec<_>>());
        let e = format!("{:#}", plan(&site(), &a, &[], &[inc]).unwrap_err());
        assert_eq!(
            e,
            "heph.core.matrix() at p/BUILD:12 would have 1,025 cells; the limit is 1,024"
        );
        // 40 axes of 2: the limit error, not an overflow or an enumeration.
        let forty: Vec<(String, &[&str])> = (0..40).map(|i| (format!("a{i}"), two)).collect();
        let forty: Vec<(&str, &[&str])> = forty.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let e = format!(
            "{:#}",
            plan(&site(), &axes(&forty).unwrap(), &[], &[]).unwrap_err()
        );
        assert!(e.contains("would have 1,099,511,627,776 cells"), "{e}");
        // 70 axes of 2 overflow a u64.
        let seventy: Vec<(String, &[&str])> = (0..70).map(|i| (format!("a{i}"), two)).collect();
        let seventy: Vec<(&str, &[&str])> = seventy.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let e = format!(
            "{:#}",
            plan(&site(), &axes(&seventy).unwrap(), &[], &[]).unwrap_err()
        );
        assert!(e.contains("would have more than"), "{e}");
    }

    #[test]
    fn host_default_picks_the_hosts_cell() {
        let a = axes(&[("platform", PLATS)]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        for (i, (os, arch)) in [("linux", "amd64"), ("linux", "arm64"), ("darwin", "arm64")]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                resolve_default(&site(), &a, &cells, &DefaultArg::Host, host(os, arch)).unwrap(),
                DefaultCell::Index(i)
            );
        }
        let got = resolve_default(
            &site(),
            &a,
            &cells,
            &DefaultArg::Host,
            host("linux", "riscv64"),
        )
        .unwrap();
        assert_eq!(
            got,
            DefaultCell::NoHostCell(
                "heph.core.matrix() at p/BUILD:12 has no cell for this host \
                 (platform=linux/riscv64). Its cells: platform=linux/amd64, platform=linux/arm64, \
                 platform=darwin/arm64. Add the host with include, or pass default={...}."
                    .to_string()
            )
        );

        // os + arch axes, in either order.
        let a = axes(&[("arch", &["amd64", "arm64"]), ("os", &["linux"])]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        assert_eq!(
            resolve_default(
                &site(),
                &a,
                &cells,
                &DefaultArg::Host,
                host("linux", "arm64")
            )
            .unwrap(),
            DefaultCell::Index(1)
        );
    }

    #[test]
    fn dict_default_and_its_structural_errors() {
        let a = axes(&[("go", &["1.22", "1.23"]), ("platform", PLATS)]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        let resolve = |e: &[(&str, &str)], h: Host| {
            resolve_default(&site(), &a, &cells, &DefaultArg::Dict(entry(e)), h)
        };
        let h = host("darwin", "arm64");
        assert_eq!(
            resolve(&[("platform", "host"), ("go", "1.23")], h).unwrap(),
            DefaultCell::Index(5)
        );
        assert_eq!(
            resolve(&[("platform", "linux/amd64"), ("go", "1.22")], h).unwrap(),
            DefaultCell::Index(0)
        );
        // The host part waits for the read; the fixed part does not.
        assert!(matches!(
            resolve(
                &[("platform", "host"), ("go", "1.23")],
                host("linux", "riscv64")
            )
            .unwrap(),
            DefaultCell::NoHostCell(_)
        ));
        let err = |e: &[(&str, &str)]| format!("{:#}", resolve(e, h).unwrap_err());
        assert!(err(&[("platform", "host"), ("go", "1.21")]).contains("names no cell"));
        assert!(err(&[("platform", "host")]).contains("gives no value for axis go"));
        assert!(
            err(&[("platform", "host"), ("go", "1.23"), ("os", "x")]).contains("names axis \"os\"")
        );
        assert!(err(&[("platform", "host"), ("go", "host")]).contains("only an axis named"));

        // "host" with a third axis says how to write a dict default.
        let e = format!(
            "{:#}",
            resolve_default(&site(), &a, &cells, &DefaultArg::Host, h).unwrap_err()
        );
        assert!(
            e.contains(
                "Pass a dict instead: default = {\"go\": \"<value>\", \"platform\": \"host\"}"
            ),
            "{e}"
        );
        // A platform value without `/`.
        let a = axes(&[("platform", &["linux-amd64"])]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        let e = format!(
            "{:#}",
            resolve_default(&site(), &a, &cells, &DefaultArg::Host, h).unwrap_err()
        );
        assert!(e.contains("has no `/`"), "{e}");
    }

    /// Evaluate package `pkg` of a workspace holding `files`, on `host`. Every
    /// BUILD file may `load("//t/eq.star", "eq")` for assertions.
    fn eval_pkg(
        files: &[(&str, &str)],
        pkg: &str,
        h: Host,
    ) -> anyhow::Result<Arc<crate::pluginbuildfile::run_file::RunResult>> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let all = files.iter().copied().chain([(
            "t/eq.star",
            "def eq(got, want):\n    if got != want:\n        fail(\"want %r, got %r\" % (want, got))\n",
        )]);
        for (path, content) in all {
            let p = tmp.path().join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, content).expect("write");
        }
        crate::pluginbuildfile::run_file::tests::source_loader(tmp.path().to_path_buf())
            .with_host(h)
            .load_pkg(pkg)
    }

    /// `p/BUILD` holding `eq`'s load on line 1, then `body` from line 2.
    fn eval(
        body: &str,
        h: Host,
    ) -> anyhow::Result<Arc<crate::pluginbuildfile::run_file::RunResult>> {
        let src = format!("load(\"//t/eq.star\", \"eq\")\n{body}");
        eval_pkg(&[("p/BUILD", &src)], "p", h)
    }

    fn eval_err(body: &str, h: Host) -> String {
        match eval(body, h) {
            Ok(_) => panic!("evaluated:\n{body}"),
            Err(e) => format!("{e:#}"),
        }
    }

    fn darwin() -> Host {
        host("darwin", "arm64")
    }

    /// T1.
    #[test]
    fn matrix_keys_and_cells() {
        eval(
            r#"
ONE = heph.core.matrix(axes = {"platform": ["linux/amd64", "darwin/arm64"]})
eq(ONE.cells, {"linux/amd64": {"platform": "linux/amd64"}, "darwin/arm64": {"platform": "darwin/arm64"}})
eq(list(ONE.cells.keys()), ["linux/amd64", "darwin/arm64"])

TWO = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "race": ["on", "off"]},
    cell = lambda go, race: go + "-" + race,
)
eq(list(TWO.cells.keys()), [("1.22", "on"), ("1.22", "off"), ("1.23", "on"), ("1.23", "off")])
eq(TWO.cells[("1.23", "off")], "1.23-off")
eq(heph.core.matrix(axes = {"go": ["1.23"], "race": ["on"]}).cells[("1.23", "on")], {"go": "1.23", "race": "on"})

THREE = heph.core.matrix(
    axes = {"a": ["1", "2"], "b": ["x"], "c": ["p", "q"]},
    cell = lambda a, b, c: a + b + c,
    exclude = [{"a": "1"}, {"a": "1", "c": "p"}],
    include = [{"c": "p", "b": "x", "a": "1"}, {"a": "3", "b": "y", "c": "r"}],
)
eq(list(THREE.cells.items()), [
    (("2", "x", "p"), "2xp"),
    (("2", "x", "q"), "2xq"),
    (("1", "x", "p"), "1xp"),
    (("3", "y", "r"), "3yr"),
])
eq(heph.core.matrix(axes = {"a": ["1"]}, exclude = [{"a": "1"}]).cells, {})
"#,
            darwin(),
        )
        .expect("evaluates");
    }

    /// T2.
    #[test]
    fn matrix_refuses_bad_input() {
        let cases: &[(&str, &str)] = &[
            ("heph.core.matrix(axes = {})", "`axes` is empty"),
            ("heph.core.matrix(axes = [])", "`axes` must be a dict"),
            (r#"heph.core.matrix(axes = {"go": []})"#, "has no values"),
            (
                r#"heph.core.matrix(axes = {"go": "1.23"})"#,
                "must be a list of strings, got string",
            ),
            (
                r#"heph.core.matrix(axes = {"go": 1})"#,
                "must be a list of strings, got int",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ("1",)})"#,
                "must be a list of strings, got tuple",
            ),
            (
                r#"heph.core.matrix(axes = {"go": {"1": "2"}})"#,
                "must be a list of strings, got dict",
            ),
            (
                r#"heph.core.matrix(axes = {"go": [1]})"#,
                "must hold strings, got int",
            ),
            (
                r#"heph.core.matrix(axes = {1: ["a"]})"#,
                "an axis name must be a string",
            ),
            (
                r#"heph.core.matrix(axes = {"go-version": ["1"]})"#,
                "not a Starlark identifier",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1 2"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["a@b"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["a:b"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["a|b"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["a\nb"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["é"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["１"]})"#,
                "is not allowed",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1", "1"]})"#,
                "lists \"1\" twice",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = [{"os": "x"}])"#,
                "exclude names axis \"os\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = [{"go": "2"}])"#,
                "matches no cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = {"go": "1"})"#,
                "`exclude` must be a list of dicts",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"], "r": ["a"]}, include = [{"go": "2"}])"#,
                "gives no value for axis r",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{"go": "2", "os": "x"}])"#,
                "names axis \"os\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{"go": "1"}])"#,
                "is already a cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{"go": 2}])"#,
                "must map axis names to string values",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, cell = 1)"#,
                "`cell` must be callable, got int",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = "host")"#,
                "default = \"host\" needs one axis named platform",
            ),
            (
                r#"heph.core.matrix(axes = {"platform": ["linux"]}, default = "host")"#,
                "has no `/`",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"], "platform": ["linux/amd64"]}, default = "host")"#,
                "Pass a dict instead",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"os": "x", "go": "1"})"#,
                "names axis \"os\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"], "r": ["a"]}, default = {"go": "1"})"#,
                "gives no value for axis r",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"go": "2"})"#,
                "names no cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = "linux")"#,
                "`default` must be None, \"host\" or a dict",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = 1)"#,
                "`default` must be None, \"host\" or a dict",
            ),
        ];
        for (call, want) in cases {
            let err = eval_err(call, darwin());
            assert!(
                err.contains("heph.core.matrix() at p/BUILD:2") && err.contains(want),
                "{call}\nwant {want:?} at p/BUILD:2, got:\n{err}"
            );
        }

        // A cell whose parameters do not match the axes fails at the first
        // call, naming the call site, the cell and the parameter.
        let err = eval_err(
            "def _c(go):\n    return go\nheph.core.matrix(axes = {\"platform\": [\"linux/amd64\"]}, cell = _c)",
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:4: cell platform=linux/amd64")
                && err.contains("platform"),
            "{err}"
        );
        // `**kwargs` takes whatever the axes are.
        eval(
            "eq(heph.core.matrix(axes = {\"a\": [\"1\"], \"b\": [\"2\"]}, cell = lambda **kw: kw).cells, {(\"1\", \"2\"): {\"a\": \"1\", \"b\": \"2\"}})",
            darwin(),
        )
        .expect("**kwargs");
        // `..`, `/` and `a//b` are values; their D8 names are valid target names.
        let result = eval(
            "heph.core.matrix(axes = {\"v\": [\"..\", \"/\", \"a//b\"]}, cell = lambda v: target(name = \"t.\" + v.replace(\"/\", \"-\"), driver = \"d\"))",
            darwin(),
        )
        .expect("odd values");
        let names: Vec<&str> = result.targets().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["t...", "t.-", "t.a--b"]);
        for n in names {
            let addr = format!("//p:{n}");
            assert_eq!(
                hmodel::htaddr::parse_addr(&addr).expect("parses").format(),
                addr,
                "{n} round-trips"
            );
        }
    }

    /// T3.
    #[test]
    fn matrix_returns_cell_values_verbatim() {
        eval(
            r#"
V = {"a": "s", "b": ":rel", "c": {"bin": ":x"}, "d": [1, ":y"], "e": None}
M = heph.core.matrix(axes = {"k": ["a", "b", "c", "d", "e"]}, cell = lambda k: V[k])
eq(M.cells, V)
for k in V:
    eq(heph.core.matrix(axes = {"k": ["a", "b", "c", "d", "e"]}, cell = lambda k: V[k], default = {"k": k}).default, V[k])
"#,
            darwin(),
        )
        .expect("verbatim");

        let err = eval_err(
            r#"def _cell(platform):
    if platform == "linux/arm64":
        fail("boom")
    return platform
heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64"]}, cell = _cell)"#,
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:6: cell platform=linux/arm64"),
            "{err}"
        );
        assert!(err.contains("boom"), "{err}");
        // The cell's own traceback is kept.
        assert!(err.contains("_cell"), "{err}");
    }

    /// T6.
    #[test]
    fn matrix_default() {
        let body = r#"
M = heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]}, cell = lambda platform: ":t." + platform, default = "host")
eq(M.default, ":t." + heph.core.host())
eq(M.default, M.default)
eq(heph.core.host(), heph.core.os() + "/" + heph.core.arch())
D = heph.core.matrix(axes = {"go": ["1.22", "1.23"], "platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]}, default = {"platform": "host", "go": "1.23"})
eq(D.default, {"go": "1.23", "platform": heph.core.host()})
"#;
        for (os, arch) in [("linux", "amd64"), ("linux", "arm64"), ("darwin", "arm64")] {
            eval(body, host(os, arch)).unwrap_or_else(|e| panic!("{os}/{arch}: {e:#}"));
        }
        // The real host.
        eval(
            "eq(heph.core.host(), heph.core.os() + \"/\" + heph.core.arch())",
            Host::current(),
        )
        .expect("real host");

        // A host with no cell raises on read only, with the stated text.
        let riscv = host("linux", "riscv64");
        let m = "M = heph.core.matrix(axes = {\"platform\": [\"linux/amd64\", \"linux/arm64\", \"darwin/arm64\"]}, default = \"host\")";
        eval(&format!("{m}\neq(len(M.cells), 3)"), riscv).expect("never reads .default");
        let err = eval_err(&format!("{m}\nX = M.default"), riscv);
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2 has no cell for this host (platform=linux/riscv64). \
                 Its cells: platform=linux/amd64, platform=linux/arm64, platform=darwin/arm64. \
                 Add the host with include, or pass default={...}."
            ),
            "{err}"
        );

        // `default = None` (or omitted): no default to read.
        let err = eval_err(
            "M = heph.core.matrix(axes = {\"go\": [\"1\"]}, default = None)\nX = M.default",
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:2 has no default"),
            "{err}"
        );
    }

    /// T7.
    #[test]
    fn matrix_result_through_load() {
        let defs = r#"M = heph.core.matrix(axes = {"platform": ["linux/amd64"]}, default = "host")

def make(name):
    return heph.core.matrix(
        axes = {"platform": ["linux/amd64", "darwin/arm64"]},
        cell = lambda platform: target(name = name + "." + platform.replace("/", "-"), driver = "d"),
    )
"#;
        let files = [
            ("lib/defs.star", defs),
            (
                "a/BUILD",
                "load(\"//t/eq.star\", \"eq\")\nload(\"//lib/defs.star\", \"M\")\neq(M.cells, {\"linux/amd64\": {\"platform\": \"linux/amd64\"}})\n",
            ),
            (
                "b/BUILD",
                "load(\"//lib/defs.star\", \"M\")\nX = M.default\n",
            ),
            (
                "c/BUILD",
                "load(\"//t/eq.star\", \"eq\")\nload(\"//lib/defs.star\", \"make\")\nT = make(\"tool\")\neq(T.cells[\"darwin/arm64\"], \"//c:tool.darwin-arm64\")\n",
            ),
        ];
        let riscv = host("linux", "riscv64");
        eval_pkg(&files, "a", riscv).expect("cells readable through load");
        let err = format!("{:#}", eval_pkg(&files, "b", riscv).unwrap_err());
        assert!(
            err.contains("heph.core.matrix() at lib/defs.star:1 has no cell for this host"),
            "{err}"
        );
        let c = eval_pkg(&files, "c", riscv).expect("a matrix in a def");
        let names: Vec<&str> = c.targets().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["tool.linux-amd64", "tool.darwin-arm64"]);
    }

    /// T13.
    #[test]
    fn matrix_duplicate_names() {
        let err = eval_err(
            r#"def _c(platform):
    return target(name = "tool", driver = "http_fetch", url = "https://x/" + platform)
heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64"]}, cell = _c)"#,
            darwin(),
        );
        assert!(
            err.contains(
                "//p:tool is declared twice with different configs: \
                 p/BUILD:3 (cell platform=linux/amd64) and p/BUILD:3 (cell platform=linux/arm64). \
                 They differ in config.url: remove one declaration, or make them identical"
            ),
            "{err}"
        );

        // A cell reusing an earlier top-level name.
        let err = eval_err(
            r#"target(name = "tool", driver = "d", v = "top")
heph.core.matrix(axes = {"v": ["a"]}, cell = lambda v: target(name = "tool", driver = "d", v = v))"#,
            darwin(),
        );
        assert!(err.contains("p/BUILD:2 and p/BUILD:3 (cell v=a)"), "{err}");

        // D8 names that collide: `linux/arm64` and `linux-arm64`.
        let err = eval_err(
            r#"heph.core.matrix(axes = {"platform": ["linux/arm64", "linux-arm64"]}, cell = lambda platform: target(name = "t." + platform.replace("/", "-"), driver = "d", p = platform))"#,
            darwin(),
        );
        assert!(
            err.contains("(cell platform=linux/arm64) and p/BUILD:2 (cell platform=linux-arm64)"),
            "{err}"
        );

        // The same declaration from every cell collapses to one target.
        let result = eval(
            r#"heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64"]}, cell = lambda platform: target(name = "shared", driver = "d"))"#,
            darwin(),
        )
        .expect("identical declarations");
        assert_eq!(result.targets().len(), 1);
    }

    /// T14.
    #[test]
    fn matrix_limits() {
        let err = eval_err(
            "heph.core.matrix(axes = {\"v\": [str(i) for i in range(1025)]}, cell = lambda v: fail(\"ran\"))",
            darwin(),
        );
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2 would have 1,025 cells; the limit is 1,024"
            ) && !err.contains("fail: ran"),
            "{err}"
        );
        let err = eval_err(
            "heph.core.matrix(axes = {\"a%d\" % i: [\"x\", \"y\"] for i in range(40)}, cell = lambda **kw: fail(\"ran\"))",
            darwin(),
        );
        assert!(
            err.contains("would have 1,099,511,627,776 cells; the limit is 1,024"),
            "{err}"
        );
        eval(
            "eq(len(heph.core.matrix(axes = {\"v\": [str(i) for i in range(1024)]}, cell = lambda v: None).cells), 1024)",
            darwin(),
        )
        .expect("exactly the limit");

        let err = eval_err(
            "heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = lambda a: heph.core.matrix(axes = {\"b\": [\"2\"]}))",
            darwin(),
        );
        assert!(err.contains("heph.core.matrix() inside a cell"), "{err}");
        // A cell passing itself ends in the same error, not a stack overflow.
        let err = eval_err(
            "def _c(a):\n    return heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = _c)\n_c(\"x\")",
            darwin(),
        );
        assert!(err.contains("heph.core.matrix() inside a cell"), "{err}");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1024), "1,024");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
