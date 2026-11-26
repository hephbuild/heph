//! `heph.core.matrix()`: enumerate the cells of a matrix, call a cell function
//! once per cell, and return what each call returned keyed by cell, plus the
//! value of a default cell. See `docs/MATRIX.md`.
//!
//! The builtin declares nothing and never reads what a cell returns (I1, I3):
//! a target a cell declares is exactly what the cell called, as if the BUILD
//! file had written the same calls on that line. Keys and cell order are a pure
//! function of `axes`, `exclude` and `include` (I4). `default` is a dict of
//! fixed axis values, checked at the call like every other argument: the
//! builtin never reads the host (`default = {"platform": heph.core.host()}` is
//! the BUILD file reading it).
//!
//! The planning half (validation, the cross product, `exclude`/`include`, the
//! default) is plain Rust over strings, so it is tested without an evaluator;
//! [`matrix`] is the Starlark half.

use crate::pluginbuildfile::run_file::{Extra, render_span};
use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef, DictType};
use starlark::values::list::{ListRef, ListType};
use starlark::values::none::NoneOr;
use starlark::values::tuple::AllocTuple;
use starlark::values::typing::{StarlarkCallable, StarlarkCallableParamAny};
use starlark::values::{
    AllocValue, Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue,
    Trace, UnpackValue, Value, ValueLike, starlark_value,
};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// The most cells one call may have, counted as the cross product (before
/// `exclude`) plus `include`, before the first cell runs (I10). A constant, not
/// configuration: real matrices are 3 to about 120 cells, and raising it later
/// breaks nothing.
pub(crate) const MAX_CELLS: u64 = 1024;

/// How many cells the "default names no cell" error lists before eliding.
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
    /// Validates the axes. The cross product is checked against the limit
    /// before any value is, so the work here is bounded by [`MAX_CELLS`]
    /// whatever the input (I10).
    pub(crate) fn new(site: &Site, entries: Vec<(String, Vec<String>)>) -> anyhow::Result<Self> {
        if entries.is_empty() {
            return Err(site.err("`axes` is empty; give at least one axis"));
        }
        check_count(site, entries.iter().map(|(_, v)| v.len()), 0)?;
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
            let mut seen: HashSet<&str> = HashSet::with_capacity(vals.len());
            for v in &vals {
                validate_value(site, &name, v)?;
                if !seen.insert(v) {
                    return Err(site.err(format_args!("axis {name:?} lists {v:?} twice")));
                }
            }
            drop(seen);
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

/// Refuses a matrix of more than [`MAX_CELLS`] cells: the product of the axis
/// lengths, multiplied with overflow checks, plus `include`. Counted before
/// `exclude`: the count a run would pay for, not what is left after it.
pub(crate) fn check_count(
    site: &Site,
    lens: impl IntoIterator<Item = usize>,
    include: usize,
) -> anyhow::Result<()> {
    let total = lens
        .into_iter()
        .try_fold(1u64, |acc, n| acc.checked_mul(n as u64))
        .and_then(|p| p.checked_add(include as u64));
    match total {
        Some(n) if n <= MAX_CELLS => Ok(()),
        Some(n) => Err(anyhow::anyhow!(
            "heph.core.matrix() at {site} would have {} cells; the limit is {}",
            thousands(n),
            thousands(MAX_CELLS),
        )),
        None => Err(anyhow::anyhow!(
            "heph.core.matrix() at {site} would have more than {} cells; the limit is {}",
            thousands(u64::MAX),
            thousands(MAX_CELLS),
        )),
    }
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
    check_count(site, axes.values.iter().map(Vec::len), include.len())?;

    // An exclude may name a subset of the axes, only axes that exist, and must
    // match a cell of the full cross product: a value outside its axis is a
    // stale exclude, which is a typo. An empty one would match every cell.
    let mut excludes: Vec<Vec<(usize, &str)>> = Vec::with_capacity(exclude.len());
    for e in exclude {
        if e.is_empty() {
            return Err(site.err(
                "exclude {} names no axis, so it would remove every cell; an exclude entry \
                 names at least one axis",
            ));
        }
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
    // the axis list (which it does not extend). It may add back an excluded
    // cell, never one still there.
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

/// The index of the cell `default` names. Every mistake fails here, at the
/// call: an axis that does not exist, one left out, or values naming no cell
/// (which is also what a host-derived dict on a host with no cell is).
pub(crate) fn resolve_default(
    site: &Site,
    axes: &Axes,
    cells: &[Cell],
    entry: &Entry,
) -> anyhow::Result<usize> {
    let mut wanted: Vec<Option<&str>> = vec![None; axes.names.len()];
    for (axis, value) in entry {
        let Some(i) = axes.index(axis) else {
            return Err(site.err(format_args!(
                "default names axis {axis:?}, which is not an axis of this matrix ({})",
                axes.known()
            )));
        };
        if let Some(slot) = wanted.get_mut(i) {
            *slot = Some(value);
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
            "default gives no value for axis {}; a default names every axis",
            missing.join(", ")
        )));
    }
    let wanted: Vec<&str> = wanted.into_iter().flatten().collect();
    if let Some(i) = cells
        .iter()
        .position(|c| c.iter().map(String::as_str).eq(wanted.iter().copied()))
    {
        return Ok(i);
    }
    let mut listed: Vec<String> = cells
        .iter()
        .take(LISTED_CELLS)
        .map(|c| describe(&axes.names, c))
        .collect();
    if cells.len() > LISTED_CELLS {
        listed.push(format!("and {} more", cells.len() - LISTED_CELLS));
    }
    let wanted: Vec<String> = wanted.into_iter().map(str::to_string).collect();
    Err(site.err(format_args!(
        "default {{{}}} names no cell of this matrix. Its cells: {}. Add it with include, or \
         name another cell",
        describe(&axes.names, &wanted),
        if listed.is_empty() {
            "none".to_string()
        } else {
            listed.join(", ")
        }
    )))
}

/// The value `heph.core.matrix()` returns. `cells` is always an attribute;
/// `default` is one only when the call named a default cell. The default's
/// value is stored at the call, never looked up in `cells`, which BUILD code
/// can mutate before freezing (D16).
#[derive(Debug, Trace, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct MatrixGen<V> {
    cells: V,
    default: Option<V>,
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
        })
    }
}

impl<'v> AllocValue<'v> for MatrixGen<Value<'v>> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

/// `cells`: what each cell's function returned, keyed by cell, in cell order
/// (the bare value for one axis, a tuple in axis order for several).
const CELLS: &str = "cells";
/// `default`: the default cell's value, present only with a default.
const DEFAULT: &str = "default";

#[starlark_value(type = "matrix")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for MatrixGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            CELLS => Some(self.cells.to_value()),
            DEFAULT => self.default.map(ValueLike::to_value),
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        match attribute {
            CELLS => true,
            DEFAULT => self.default.is_some(),
            _ => false,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut out = vec![CELLS.to_string()];
        if self.default.is_some() {
            out.push(DEFAULT.to_string());
        }
        out
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
    Site(render_span(root, eval.call_stack_top_location().as_ref()))
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
/// `None | dict[str, str]`.
pub(crate) type DefaultTy = NoneOr<DictType<String, String>>;
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
            site.err("a cell may not call heph.core.matrix()"),
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
    let exclude = unpack_entries(&site, "exclude", args.exclude)?;
    let include = unpack_entries(&site, "include", args.include)?;
    // The whole count, include too, before a value is validated (I10).
    check_count(&site, entries.iter().map(|(_, v)| v.len()), include.len())
        .map_err(starlark::Error::new_other)?;
    let axes = Axes::new(&site, entries).map_err(starlark::Error::new_other)?;
    let cells = plan(&site, &axes, &exclude, &include).map_err(starlark::Error::new_other)?;

    let default_cell = match args.default.filter(|v| !v.is_none()) {
        None => None,
        Some(v) if DictRef::from_value(v).is_some() => {
            let entry = unpack_entry(&site, "`default`", v)?;
            Some(
                resolve_default(&site, &axes, &cells, &entry)
                    .map_err(starlark::Error::new_other)?,
            )
        }
        Some(v) => {
            return Err(starlark::Error::new_other(site.err(format_args!(
                "`default` must be None or a dict of axis to value; for the host's cell write \
                 default = {{\"platform\": heph.core.host()}} (got {})",
                v.get_type()
            ))));
        }
    };

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
                    // The label of starlark's diagnostic shows only the
                    // outermost context, so the cause is written into it:
                    // otherwise "missing parameter `go`" never reaches the user.
                    let cause = e.without_diagnostic().to_string();
                    e.with_context(format!(
                        "heph.core.matrix() at {site}: cell {desc}: {cause}"
                    ))
                })?
            }
        };
        if default_cell == Some(i) {
            default_value = Some(value);
        }
        pairs.push((key, value));
    }

    Ok(heap.alloc(MatrixGen {
        cells: heap.alloc(AllocDict(pairs)),
        default: default_value,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::Host;

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
        // Excluding every cell, entry by entry, is allowed.
        assert!(
            plan(
                &site(),
                &a,
                &[entry(&[("go", "1.22")]), entry(&[("go", "1.23")])],
                &[]
            )
            .unwrap()
            .is_empty()
        );
        // An empty entry would match every cell, and is refused.
        let e = format!("{:#}", plan(&site(), &a, &[entry(&[])], &[]).unwrap_err());
        assert_eq!(
            e,
            "heph.core.matrix() at p/BUILD:12: exclude {} names no axis, so it would remove \
             every cell; an exclude entry names at least one axis"
        );
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
        // An include's value outside its axis does not extend the axis: an
        // exclude naming it is stale.
        let e = err(
            &[entry(&[("go", "1.24")])],
            &[entry(&[("go", "1.24"), ("race", "on")])],
        );
        assert!(e.contains("\"1.24\" is not a value of axis \"go\""), "{e}");
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
        // Over the limit before any value is validated.
        let mut big: Vec<String> = (0..1024).map(|i| i.to_string()).collect();
        big.push("bad value".to_string());
        let big: Vec<&str> = big.iter().map(String::as_str).collect();
        assert!(err(&[("v", &big)]).contains("would have 1,025 cells"));
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
        let e = format!("{:#}", axes(&forty).unwrap_err());
        assert!(e.contains("would have 1,099,511,627,776 cells"), "{e}");
        // 70 axes of 2 overflow a u64.
        let seventy: Vec<(String, &[&str])> = (0..70).map(|i| (format!("a{i}"), two)).collect();
        let seventy: Vec<(&str, &[&str])> = seventy.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let e = format!("{:#}", axes(&seventy).unwrap_err());
        assert!(e.contains("would have more than"), "{e}");
    }

    #[test]
    fn default_names_one_cell_and_every_mistake_fails_at_the_call() {
        let a = axes(&[("go", &["1.22", "1.23"]), ("platform", PLATS)]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        let resolve = |e: &[(&str, &str)]| resolve_default(&site(), &a, &cells, &entry(e));
        // Dict order does not matter.
        assert_eq!(
            resolve(&[("platform", "darwin/arm64"), ("go", "1.23")]).unwrap(),
            5
        );
        assert_eq!(
            resolve(&[("go", "1.22"), ("platform", "linux/amd64")]).unwrap(),
            0
        );
        let err = |e: &[(&str, &str)]| format!("{:#}", resolve(e).unwrap_err());
        assert!(err(&[("platform", "darwin/arm64")]).contains("gives no value for axis go"));
        assert!(
            err(&[("platform", "darwin/arm64"), ("go", "1.23"), ("os", "x")])
                .contains("names axis \"os\"")
        );
        // "host" is a value like any other: no cell has it.
        assert!(err(&[("platform", "host"), ("go", "1.23")]).contains("names no cell"));
        // A host with no cell, as `{"platform": heph.core.host()}` gives it.
        let a = axes(&[("platform", PLATS)]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        assert_eq!(
            format!(
                "{:#}",
                resolve_default(
                    &site(),
                    &a,
                    &cells,
                    &entry(&[("platform", "linux/riscv64")])
                )
                .unwrap_err()
            ),
            "heph.core.matrix() at p/BUILD:12: default {platform=linux/riscv64} names no cell of \
             this matrix. Its cells: platform=linux/amd64, platform=linux/arm64, \
             platform=darwin/arm64. Add it with include, or name another cell"
        );
        // A long cell list is elided.
        let many: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let a = axes(&[("v", &many)]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        let e = format!(
            "{:#}",
            resolve_default(&site(), &a, &cells, &entry(&[("v", "x")])).unwrap_err()
        );
        assert!(e.contains("v=15, and 4 more. Add it"), "{e}");
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
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = [{}])"#,
                "exclude {} names no axis",
            ),
            (
                r#"heph.core.matrix(axes = {"platform": ["linux/amd64"]}, default = "host")"#,
                "`default` must be None or a dict of axis to value; for the host's cell write \
                 default = {\"platform\": heph.core.host()} (got string)",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"go": 1})"#,
                "`default` must map axis names to string values, got int",
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
                "`default` must be None or a dict of axis to value",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = 1)"#,
                "(got int)",
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
        // call, naming the call site and the cell, with starlark's own text
        // naming the parameter: one the axes do not give...
        let err = eval_err(
            "def _c(go, platform):\n    return go\nheph.core.matrix(axes = {\"platform\": [\"linux/amd64\"]}, cell = _c)",
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:4: cell platform=linux/amd64")
                && err.contains(": Missing parameter `go` for call to `"),
            "{err}"
        );
        // ...and an axis the cell does not take.
        let err = eval_err(
            "def _c(go):\n    return go\nheph.core.matrix(axes = {\"go\": [\"1\"], \"platform\": [\"linux/amd64\"]}, cell = _c)",
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:4: cell go=1, platform=linux/amd64")
                && err.contains(": Found `platform` extra named parameter(s) for call to "),
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
        // The host's cell, as the BUILD file names it.
        let body = r#"
M = heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]}, cell = lambda platform: ":t." + platform, default = {"platform": heph.core.host()})
eq(M.default, ":t." + heph.core.host())
eq(M.default, M.default)
eq(hasattr(M, "default"), True)
eq(dir(M), ["cells", "default"])
eq(heph.core.host(), heph.core.os() + "/" + heph.core.arch())
D = heph.core.matrix(axes = {"go": ["1.22", "1.23"], "platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]}, default = {"platform": heph.core.host(), "go": "1.23"})
eq(D.default, {"go": "1.23", "platform": heph.core.host()})
F = heph.core.matrix(axes = {"go": ["1.22", "1.23"], "race": ["on", "off"]}, default = {"race": "off", "go": "1.22"})
eq(F.default, {"go": "1.22", "race": "off"})
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

        // A host with no cell fails at the call, not at a read.
        let riscv = host("linux", "riscv64");
        let err = eval_err(
            "M = heph.core.matrix(axes = {\"platform\": [\"linux/amd64\", \"linux/arm64\", \"darwin/arm64\"]}, default = {\"platform\": heph.core.host()})",
            riscv,
        );
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2: default {platform=linux/riscv64} names no cell \
                 of this matrix. Its cells: platform=linux/amd64, platform=linux/arm64, \
                 platform=darwin/arm64. Add it with include, or name another cell"
            ),
            "{err}"
        );
        // Without a default, the host plays no part.
        eval(
            "eq(len(heph.core.matrix(axes = {\"platform\": [\"linux/amd64\"]}).cells), 1)",
            riscv,
        )
        .expect("no default, no host");

        // `default = None` (or omitted): no `default` attribute at all.
        eval(
            r#"
N = heph.core.matrix(axes = {"go": ["1"]})
eq(hasattr(N, "default"), False)
eq(hasattr(N, "cells"), True)
eq(dir(N), ["cells"])
eq(hasattr(heph.core.matrix(axes = {"go": ["1"]}, default = None), "default"), False)
eq(getattr(N, "default", "none"), "none")
"#,
            darwin(),
        )
        .expect("no default attribute");
        let err = eval_err(
            "M = heph.core.matrix(axes = {\"go\": [\"1\"]}, default = None)\nX = M.default",
            darwin(),
        );
        assert!(
            err.contains("Object of type `matrix` has no attribute `default`"),
            "{err}"
        );
    }

    /// D16: what `.default` reads is stored at the call, so mutating `cells`
    /// before the module freezes does not move it.
    #[test]
    fn matrix_default_survives_mutating_cells() {
        eval(
            r#"
M = heph.core.matrix(axes = {"k": ["a", "b"]}, cell = lambda k: k + "!", default = {"k": "a"})
M.cells["a"] = "changed"
M.cells.pop("b")
eq(M.cells, {"a": "changed"})
eq(M.default, "a!")
"#,
            darwin(),
        )
        .expect("default kept");
    }

    /// T7.
    #[test]
    fn matrix_result_through_load() {
        let defs = r#"M = heph.core.matrix(
    axes = {"platform": ["linux/amd64", "darwin/arm64"]},
    cell = lambda platform: "//lib:t." + platform.replace("/", "-"),
    default = {"platform": "darwin/arm64"},
)
N = heph.core.matrix(axes = {"platform": ["linux/amd64"]})

def make(name):
    return heph.core.matrix(
        axes = {"platform": ["linux/amd64", "darwin/arm64"]},
        cell = lambda platform: target(name = name + "." + platform.replace("/", "-"), driver = "d"),
    )
"#;
        // Both packages read the frozen value: `cells`, `default`, and
        // `hasattr` on one without a default.
        let consumer = r#"load("//t/eq.star", "eq")
load("//lib/defs.star", "M", "N")
eq(M.cells, {"linux/amd64": "//lib:t.linux-amd64", "darwin/arm64": "//lib:t.darwin-arm64"})
eq(hasattr(M, "default"), True)
eq(hasattr(N, "default"), False)
eq(dir(N), ["cells"])
target(name = "use", driver = "d", tool = M.default, all = list(M.cells.values()))
"#;
        let files = [
            ("lib/defs.star", defs),
            ("a/BUILD", consumer),
            ("b/BUILD", consumer),
            (
                "c/BUILD",
                "load(\"//t/eq.star\", \"eq\")\nload(\"//lib/defs.star\", \"make\")\nT = make(\"tool\")\neq(T.cells[\"darwin/arm64\"], \"//c:tool.darwin-arm64\")\n",
            ),
        ];
        let riscv = host("linux", "riscv64");
        let config = |pkg: &str| {
            let r = eval_pkg(&files, pkg, riscv).unwrap_or_else(|e| panic!("{pkg}: {e:#}"));
            r.target("use").expect("use").config.clone()
        };
        let (a, b) = (config("a"), config("b"));
        assert_eq!(
            a.get("tool"),
            Some(&hcore::htvalue::Value::String(
                "//lib:t.darwin-arm64".to_string()
            ))
        );
        assert_eq!(
            a.get("tool"),
            b.get("tool"),
            "`.default` is one value across packages"
        );
        assert_eq!(a.get("all"), b.get("all"));
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
                 They differ in config.url. If both come from a matrix cell, put the cell's axis \
                 values in the target name; otherwise remove one declaration, or make them \
                 identical"
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
        // ...and over two axes: `1.2` + `3` against `1` + `2.3`.
        let err = eval_err(
            r#"heph.core.matrix(axes = {"a": ["1.2", "1"], "b": ["3", "2.3"]}, cell = lambda a, b: target(name = "t." + a + "." + b, driver = "d", a = a, b = b))"#,
            darwin(),
        );
        assert!(
            err.contains(
                "//p:t.1.2.3 is declared twice with different configs: p/BUILD:2 (cell a=1.2, \
                 b=3) and p/BUILD:2 (cell a=1, b=2.3). They differ in config.a, config.b."
            ),
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
        // A huge axis is refused by its length, before any value is checked
        // (the bad last value is never reached), so validation never runs
        // over more than the limit.
        let err = eval_err(
            "heph.core.matrix(axes = {\"v\": [str(i) for i in range(200000)] + [\"bad value\"]})",
            darwin(),
        );
        assert!(
            err.contains("would have 200,001 cells; the limit is 1,024"),
            "{err}"
        );
        // `include` counts before the axes are validated, too.
        let err = eval_err(
            "heph.core.matrix(axes = {\"v\": [str(i) for i in range(1024)]}, include = [{\"v\": \"x\"}])",
            darwin(),
        );
        assert!(err.contains("would have 1,025 cells"), "{err}");

        let err = eval_err(
            "heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = lambda a: heph.core.matrix(axes = {\"b\": [\"2\"]}))",
            darwin(),
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:2: a cell may not call heph.core.matrix()"),
            "{err}"
        );
        // A cell passing itself ends in the same error, not a stack overflow.
        let err = eval_err(
            "def _c(a):\n    return heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = _c)\n_c(\"x\")",
            darwin(),
        );
        assert!(
            err.contains("a cell may not call heph.core.matrix()"),
            "{err}"
        );
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1024), "1,024");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
