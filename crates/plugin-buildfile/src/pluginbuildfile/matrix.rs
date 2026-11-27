//! `heph.core.matrix()`: the cross product of some axes, a cell function called
//! once per cell, and a list of what each call returned, in cell order, plus
//! the value of a default cell. See `docs/MATRIX.md`.
//!
//! The matrix is a generic cross product: axis names are any strings, axis
//! values any Starlark values, passed to the cell function as they are. It
//! knows nothing about targets or platforms. Values are compared by Starlark
//! equality (`exclude`, `include`, `default` and duplicate values) and shown in
//! errors by their repr.
//!
//! The builtin declares nothing and never reads what a cell returns (I1, I3):
//! a target a cell declares is exactly what the cell called, as if the BUILD
//! file had written the same calls on that line. Cell order is a pure function
//! of `axes`, `exclude` and `include` (I4).
//!
//! The planning half (validation, the cross product, `exclude`/`include`, the
//! default) is generic over [`AxisValue`], so it is tested over plain strings
//! without an evaluator; [`matrix`] is the Starlark half.

use crate::pluginbuildfile::run_file::{Extra, render_span};
use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef, DictType};
use starlark::values::list::{AllocList, ListRef, ListType};
use starlark::values::none::NoneOr;
use starlark::values::structs::AllocStruct;
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

/// How many cells the "default names no cell" error lists before eliding.
const LISTED_CELLS: usize = 16;

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

/// An axis value as planning sees it: compared by Starlark equality, shown by
/// its repr.
pub(crate) trait AxisValue: Clone {
    /// Starlark `==`. Fallible: comparing deeply nested values can hit
    /// starlark's recursion limit.
    fn same(&self, other: &Self) -> anyhow::Result<bool>;
    /// How errors show the value: `"1.23"`, `True`, `["-race"]`.
    fn repr(&self) -> String;
}

impl AxisValue for Value<'_> {
    fn same(&self, other: &Self) -> anyhow::Result<bool> {
        self.equals(*other).map_err(starlark::Error::into_anyhow)
    }

    fn repr(&self) -> String {
        self.to_repr()
    }
}

/// The index of the first of `vs` equal to `v`.
fn index_of<'a, V: AxisValue + 'a>(
    vs: impl IntoIterator<Item = &'a V>,
    v: &V,
) -> anyhow::Result<Option<usize>> {
    for (i, x) in vs.into_iter().enumerate() {
        if x.same(v)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Whether two cells (one value per axis, in axis order) are equal.
fn same_cell<V: AxisValue>(a: &[V], b: &[V]) -> anyhow::Result<bool> {
    for (x, y) in a.iter().zip(b) {
        if !x.same(y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The validated axes, in declaration order: that order is the order of a
/// cell's values, of its `axes` dict and of the cell function's arguments.
#[derive(Debug)]
pub(crate) struct Axes<V> {
    names: Vec<String>,
    values: Vec<Vec<V>>,
}

/// One cell: a value per axis, in axis order.
pub(crate) type Cell<V> = Vec<V>;

/// An `exclude`, `include` or `default` entry as written: axis to value, in
/// dict order.
pub(crate) type Entry<V> = Vec<(String, V)>;

impl<V: AxisValue> Axes<V> {
    /// Validates the axes. The cross product is checked against the limit
    /// before any value is compared, so the work here is bounded by
    /// [`MAX_CELLS`] whatever the input (I10).
    pub(crate) fn new(site: &Site, entries: Vec<(String, Vec<V>)>) -> anyhow::Result<Self> {
        if entries.is_empty() {
            return Err(site.err("`axes` is empty; give at least one axis"));
        }
        check_count(site, entries.iter().map(|(_, v)| v.len()), 0)?;
        let mut names = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());
        for (name, vals) in entries {
            if vals.is_empty() {
                return Err(site.err(format_args!("axis {name:?} has no values")));
            }
            // Quadratic, and bounded: an axis has at most MAX_CELLS values.
            // Values need not be hashable (a list is a fine value).
            for (j, v) in vals.iter().enumerate() {
                if index_of(vals.iter().take(j), v)?.is_some() {
                    return Err(site.err(format_args!("axis {name:?} lists {} twice", v.repr())));
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
        self.names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `axis=repr, axis=repr`: how errors and logs name a cell or an entry.
fn render<'a, V: AxisValue + 'a>(pairs: impl IntoIterator<Item = (&'a String, &'a V)>) -> String {
    let mut out = String::new();
    for (i, (n, v)) in pairs.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(n);
        out.push('=');
        out.push_str(&v.repr());
    }
    out
}

/// `go="1.23", race=True`: how errors and logs name a cell.
pub(crate) fn describe<V: AxisValue>(names: &[String], cell: &[V]) -> String {
    render(names.iter().zip(cell))
}

/// `{go="1.23", race=True}`: how errors name an entry as written.
fn describe_entry<V: AxisValue>(e: &Entry<V>) -> String {
    format!("{{{}}}", render(e.iter().map(|(n, v)| (n, v))))
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
pub(crate) fn plan<V: AxisValue>(
    site: &Site,
    axes: &Axes<V>,
    exclude: &[Entry<V>],
    include: &[Entry<V>],
) -> anyhow::Result<Vec<Cell<V>>> {
    check_count(site, axes.values.iter().map(Vec::len), include.len())?;

    // An exclude may name a subset of the axes, only axes that exist, and must
    // match a cell of the full cross product: a value outside its axis is a
    // stale exclude, which is a typo. An empty one would match every cell.
    // Each is resolved to (axis, value index) once, so the product compares
    // indices, not values.
    let mut excludes: Vec<Vec<(usize, usize)>> = Vec::with_capacity(exclude.len());
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
            let Some(j) = index_of(axes.values.get(i).into_iter().flatten(), value)? else {
                return Err(site.err(format_args!(
                    "exclude {} matches no cell: {} is not a value of axis {axis:?}",
                    describe_entry(e),
                    value.repr()
                )));
            };
            m.push((i, j));
        }
        excludes.push(m);
    }

    let mut cells: Vec<Cell<V>> = Vec::new();
    let mut at = vec![0usize; axes.values.len()];
    'product: loop {
        let excluded = excludes
            .iter()
            .any(|m| m.iter().all(|&(i, j)| at.get(i) == Some(&j)));
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
        let mut cell: Vec<Option<V>> = vec![None; axes.names.len()];
        for (axis, value) in e {
            let Some(i) = axes.index(axis) else {
                return Err(site.err(format_args!(
                    "include {} names axis {axis:?}, which is not an axis of this matrix ({})",
                    describe_entry(e),
                    axes.known()
                )));
            };
            if let Some(slot) = cell.get_mut(i) {
                *slot = Some(value.clone());
            }
        }
        let missing: Vec<String> = axes
            .names
            .iter()
            .zip(&cell)
            .filter(|(_, v)| v.is_none())
            .map(|(n, _)| format!("{n:?}"))
            .collect();
        if !missing.is_empty() {
            return Err(site.err(format_args!(
                "include {} gives no value for axis {}; an include names every axis",
                describe_entry(e),
                missing.join(", ")
            )));
        }
        let cell: Cell<V> = cell.into_iter().flatten().collect();
        for c in &cells {
            if same_cell(c, &cell)? {
                return Err(site.err(format_args!(
                    "include {} is already a cell of this matrix",
                    describe_entry(e)
                )));
            }
        }
        cells.push(cell);
    }
    Ok(cells)
}

/// The index of the cell `default` names. Every mistake fails here, at the
/// call: an axis that does not exist, one left out, or values naming no cell.
pub(crate) fn resolve_default<V: AxisValue>(
    site: &Site,
    axes: &Axes<V>,
    cells: &[Cell<V>],
    entry: &Entry<V>,
) -> anyhow::Result<usize> {
    let mut wanted: Vec<Option<V>> = vec![None; axes.names.len()];
    for (axis, value) in entry {
        let Some(i) = axes.index(axis) else {
            return Err(site.err(format_args!(
                "default names axis {axis:?}, which is not an axis of this matrix ({})",
                axes.known()
            )));
        };
        if let Some(slot) = wanted.get_mut(i) {
            *slot = Some(value.clone());
        }
    }
    let missing: Vec<String> = axes
        .names
        .iter()
        .zip(&wanted)
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| format!("{n:?}"))
        .collect();
    if !missing.is_empty() {
        return Err(site.err(format_args!(
            "default gives no value for axis {}; a default names every axis",
            missing.join(", ")
        )));
    }
    let wanted: Cell<V> = wanted.into_iter().flatten().collect();
    for (i, c) in cells.iter().enumerate() {
        if same_cell(c, &wanted)? {
            return Ok(i);
        }
    }
    let mut listed: Vec<String> = cells
        .iter()
        .take(LISTED_CELLS)
        .map(|c| format!("{{{}}}", describe(&axes.names, c)))
        .collect();
    if cells.len() > LISTED_CELLS {
        listed.push(format!("and {} more", cells.len() - LISTED_CELLS));
    }
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

/// `cells`: a list, in cell order, of one struct per cell.
const CELLS: &str = "cells";
/// `default`: the default cell's value, present only with a default.
const DEFAULT: &str = "default";
/// A cell's `axes`: its dict of axis name to value, in axis order.
const CELL_AXES: &str = "axes";
/// A cell's `value`: what the cell function returned (without one, the
/// `axes` dict).
const CELL_VALUE: &str = "value";

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

/// A dict of axis name to any value, in dict order.
fn unpack_entry<'v>(site: &Site, what: &str, v: Value<'v>) -> starlark::Result<Entry<Value<'v>>> {
    let Some(d) = DictRef::from_value(v) else {
        return Err(type_err(
            site,
            &format!("{what} must be a dict of axis name to value"),
            v,
        ));
    };
    d.iter()
        .map(|(k, val)| match k.unpack_str() {
            Some(k) => Ok((k.to_string(), val)),
            None => Err(type_err(
                site,
                &format!("{what} must map axis names (strings) to values"),
                k,
            )),
        })
        .collect()
}

fn unpack_entries<'v>(
    site: &Site,
    what: &str,
    v: Option<Value<'v>>,
) -> starlark::Result<Vec<Entry<Value<'v>>>> {
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
/// `dict[str, list[typing.Any]]`, which starlark prints `dict[str, list]`.
pub(crate) type AxesTy = DictType<String, ListType<FrozenValue>>;
/// `None | typing.Callable`.
pub(crate) type CellTy<'v> = NoneOr<StarlarkCallable<'v>>;
/// `None | dict[str, typing.Any]`.
pub(crate) type DefaultTy = NoneOr<DictType<String, FrozenValue>>;
/// `None | list[dict[str, typing.Any]]`.
pub(crate) type EntriesTy = NoneOr<ListType<DictType<String, FrozenValue>>>;

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
    let lists = axes_dict
        .iter()
        .map(|(k, v)| {
            let Some(name) = k.unpack_str() else {
                return Err(type_err(&site, "an axis name must be a string", k));
            };
            let Some(list) = ListRef::from_value(v) else {
                return Err(type_err(
                    &site,
                    &format!("axis {name:?} must be a list of values"),
                    v,
                ));
            };
            Ok((name.to_string(), list))
        })
        .collect::<starlark::Result<Vec<_>>>()?;
    drop(axes_dict);
    // The whole count, include too, before a value is copied or compared
    // (I10).
    let include_len = args
        .include
        .and_then(ListRef::from_value)
        .map_or(0, |l| l.content().len());
    check_count(&site, lists.iter().map(|(_, l)| l.len()), include_len)
        .map_err(starlark::Error::new_other)?;
    let entries = lists
        .into_iter()
        .map(|(name, l)| (name, l.content().to_vec()))
        .collect();
    let exclude = unpack_entries(&site, "exclude", args.exclude)?;
    let include = unpack_entries(&site, "include", args.include)?;
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
                "`default` must be None or a dict of axis name to value (got {})",
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
    let axes_dict = |cell: &[Value<'v>]| {
        heap.alloc(AllocDict(
            axes.names
                .iter()
                .map(String::as_str)
                .zip(cell.iter().copied()),
        ))
    };
    let mut out: Vec<Value<'v>> = Vec::with_capacity(cells.len());
    let mut default_value = None;
    for (i, cell) in cells.iter().enumerate() {
        let value = match cell_fn {
            None => axes_dict(cell),
            Some(f) => {
                let desc = describe(&axes.names, cell);
                *extra.cell.borrow_mut() = Some(Arc::from(desc.as_str()));
                let _guard = CellGuard(extra);
                let named: Vec<(&str, Value<'v>)> = axes
                    .names
                    .iter()
                    .map(String::as_str)
                    .zip(cell.iter().copied())
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
        out.push(heap.alloc(AllocStruct([
            (CELL_AXES, axes_dict(cell)),
            (CELL_VALUE, value),
        ])));
    }

    Ok(heap.alloc(MatrixGen {
        cells: heap.alloc(AllocList(out)),
        default: default_value,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Planning over plain strings: `==`, shown quoted as starlark shows a
    /// string.
    impl AxisValue for String {
        fn same(&self, other: &Self) -> anyhow::Result<bool> {
            Ok(self == other)
        }

        fn repr(&self) -> String {
            format!("{self:?}")
        }
    }

    fn site() -> Site {
        Site("p/BUILD:12".to_string())
    }

    fn axes(entries: &[(&str, &[&str])]) -> anyhow::Result<Axes<String>> {
        Axes::new(
            &site(),
            entries
                .iter()
                .map(|(n, vs)| (n.to_string(), vs.iter().map(|v| v.to_string()).collect()))
                .collect(),
        )
    }

    fn entry(pairs: &[(&str, &str)]) -> Entry<String> {
        pairs
            .iter()
            .map(|(a, v)| (a.to_string(), v.to_string()))
            .collect()
    }

    fn cells_of(cells: &[Cell<String>]) -> Vec<String> {
        cells.iter().map(|c| c.join(" ")).collect()
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
        let err = |ex: &[Entry<String>], inc: &[Entry<String>]| {
            format!("{:#}", plan(&site(), &a, ex, inc).unwrap_err())
        };
        let e = err(&[entry(&[("os", "linux")])], &[]);
        assert_eq!(
            e,
            "heph.core.matrix() at p/BUILD:12: exclude names axis \"os\", which is not an axis \
             of this matrix (\"go\", \"race\")"
        );
        let e = err(&[entry(&[("go", "1.21")])], &[]);
        assert_eq!(
            e,
            "heph.core.matrix() at p/BUILD:12: exclude {go=\"1.21\"} matches no cell: \"1.21\" \
             is not a value of axis \"go\""
        );
        // An include's value outside its axis does not extend the axis: an
        // exclude naming it is stale.
        let e = err(
            &[entry(&[("go", "1.24")])],
            &[entry(&[("go", "1.24"), ("race", "on")])],
        );
        assert!(e.contains("\"1.24\" is not a value of axis \"go\""), "{e}");
        let e = err(&[], &[entry(&[("go", "1.22")])]);
        assert!(
            e.contains("include {go=\"1.22\"} gives no value for axis \"race\""),
            "{e}"
        );
        let e = err(
            &[],
            &[entry(&[("go", "1.22"), ("race", "on"), ("os", "x")])],
        );
        assert!(e.contains("names axis \"os\""), "{e}");
        let e = err(&[], &[entry(&[("race", "on"), ("go", "1.22")])]);
        assert!(
            e.contains("include {race=\"on\", go=\"1.22\"} is already a cell"),
            "{e}"
        );
        // Two equal includes: the second duplicates the first.
        let inc = entry(&[("go", "1.24"), ("race", "on")]);
        let e = err(&[], &[inc.clone(), inc]);
        assert!(e.contains("is already a cell"), "{e}");
    }

    #[test]
    fn axes_refuse_empty_and_repeated_values() {
        let err = |entries: &[(&str, &[&str])]| format!("{:#}", axes(entries).unwrap_err());
        assert!(err(&[]).contains("`axes` is empty"));
        assert!(err(&[("go", &[])]).contains("axis \"go\" has no values"));
        assert!(err(&[("go", &["1", "2", "1"])]).contains("axis \"go\" lists \"1\" twice"));
        // Any string is a name, and any string a value.
        for name in ["go-version", "if", "1go", "with space", "", "é"] {
            assert!(axes(&[(name, &["a b", "", "é"])]).is_ok(), "{name:?}");
        }
        // Over the limit before any value is compared: the repeated last
        // value is never reached.
        let mut big: Vec<String> = (0..1024).map(|i| i.to_string()).collect();
        big.push("0".to_string());
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
        let a = axes(&[("go", &["1.22", "1.23"]), ("race", &["on", "off", "auto"])]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        let resolve = |e: &[(&str, &str)]| resolve_default(&site(), &a, &cells, &entry(e));
        // Dict order does not matter.
        assert_eq!(resolve(&[("race", "auto"), ("go", "1.23")]).unwrap(), 5);
        assert_eq!(resolve(&[("go", "1.22"), ("race", "on")]).unwrap(), 0);
        let err = |e: &[(&str, &str)]| format!("{:#}", resolve(e).unwrap_err());
        assert!(err(&[("race", "on")]).contains("gives no value for axis \"go\""));
        assert!(err(&[("race", "on"), ("go", "1.23"), ("os", "x")]).contains("names axis \"os\""));
        let a = axes(&[("race", &["on", "off"])]).unwrap();
        let cells = plan(&site(), &a, &[], &[]).unwrap();
        assert_eq!(
            format!(
                "{:#}",
                resolve_default(&site(), &a, &cells, &entry(&[("race", "auto")])).unwrap_err()
            ),
            "heph.core.matrix() at p/BUILD:12: default {race=\"auto\"} names no cell of this \
             matrix. Its cells: {race=\"on\"}, {race=\"off\"}. Add it with include, or name \
             another cell"
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
        assert!(e.contains("{v=\"15\"}, and 4 more. Add it"), "{e}");
    }

    /// Evaluate package `pkg` of a workspace holding `files`. Every BUILD file
    /// may `load("//t/eq.star", "eq")` for assertions.
    fn eval_pkg(
        files: &[(&str, &str)],
        pkg: &str,
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
            .load_pkg(pkg)
    }

    /// `p/BUILD` holding `eq`'s load on line 1, then `body` from line 2.
    fn eval(body: &str) -> anyhow::Result<Arc<crate::pluginbuildfile::run_file::RunResult>> {
        let src = format!("load(\"//t/eq.star\", \"eq\")\n{body}");
        eval_pkg(&[("p/BUILD", &src)], "p")
    }

    fn eval_err(body: &str) -> String {
        match eval(body) {
            Ok(_) => panic!("evaluated:\n{body}"),
            Err(e) => format!("{e:#}"),
        }
    }

    /// T1: `cells` is a list in cell order (last axis fastest), each entry a
    /// struct of `.axes` (in axis declaration order) and `.value`.
    #[test]
    fn matrix_cells_order_and_shape() {
        eval(
            r#"
M = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "race": [True, False]},
    cell = lambda go, race: go + ("-race" if race else ""),
)
eq(type(M.cells), "list")
eq(type(M.cells[0]), "struct")
eq([c.axes for c in M.cells], [
    {"go": "1.22", "race": True},
    {"go": "1.22", "race": False},
    {"go": "1.23", "race": True},
    {"go": "1.23", "race": False},
])
eq([c.value for c in M.cells], ["1.22-race", "1.22", "1.23-race", "1.23"])
eq(list(M.cells[0].axes.keys()), ["go", "race"])
eq(list(heph.core.matrix(axes = {"race": [True], "go": ["1.23"]}).cells[0].axes.keys()), ["race", "go"])

# Without `cell`, a cell's value is its axes dict.
N = heph.core.matrix(axes = {"go": ["1.23"], "race": [False]})
eq(N.cells[0].value, {"go": "1.23", "race": False})
eq(N.cells[0].value, N.cells[0].axes)

THREE = heph.core.matrix(
    axes = {"a": [1, 2], "b": ["x"], "c": ["p", "q"]},
    cell = lambda a, b, c: "%d%s%s" % (a, b, c),
    exclude = [{"a": 1}, {"a": 1, "c": "p"}],
    include = [{"c": "p", "b": "x", "a": 1}, {"a": 3, "b": "y", "c": "r"}],
)
eq([c.value for c in THREE.cells], ["2xp", "2xq", "1xp", "3yr"])
eq(list(THREE.cells[2].axes.items()), [("a", 1), ("b", "x"), ("c", "p")])
eq(heph.core.matrix(axes = {"a": [1]}, exclude = [{"a": 1}]).cells, [])
"#,
        )
        .expect("evaluates");
    }

    /// An axis name is any string. One a `def` cannot take as a parameter
    /// (`go-version`, `if`) reaches a `**kwargs` cell, and every entry.
    #[test]
    fn matrix_axis_names_are_any_string() {
        eval(
            r#"
M = heph.core.matrix(
    axes = {"go-version": ["1.22", "1.23"], "if": [1], "with space": [None], "": [0]},
    cell = lambda **kw: kw,
)
eq([c.value for c in M.cells], [
    {"go-version": "1.22", "if": 1, "with space": None, "": 0},
    {"go-version": "1.23", "if": 1, "with space": None, "": 0},
])
eq(M.cells[1].axes, M.cells[1].value)
D = heph.core.matrix(
    axes = {"go-version": ["1.22", "1.23"], "lambda": [True]},
    cell = lambda **kw: kw["go-version"] + ("!" if kw["lambda"] else ""),
    exclude = [{"go-version": "1.22"}],
    include = [{"lambda": True, "go-version": "1.24"}],
    default = {"go-version": "1.24", "lambda": True},
)
eq([c.value for c in D.cells], ["1.23!", "1.24!"])
eq(D.default, "1.24!")
"#,
        )
        .expect("any string");
        // A cell without `**kwargs` cannot take it: starlark's own text says
        // so.
        let err =
            eval_err("heph.core.matrix(axes = {\"go-version\": [\"1\"]}, cell = lambda: None)");
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:2: cell go-version=\"1\"")
                && err.contains("Found `go-version` extra named parameter(s)"),
            "{err}"
        );
    }

    /// An axis value is any Starlark value, passed to `cell` and kept in
    /// `.axes` as it is.
    #[test]
    fn matrix_values_are_any_starlark_value() {
        eval(
            r#"
S = heph.core.matrix(axes = {"s": [1]}).cells[0]
eq(type(S), "struct")
V = [1, None, ("a", 2), [1, 2], {"k": "v"}, S, 2.5, True, "s"]
M = heph.core.matrix(axes = {"v": V}, cell = lambda v: v)
eq([c.value for c in M.cells], V)
eq([c.axes["v"] for c in M.cells], V)
eq(M.cells[5].value.axes, {"s": 1})
eq(heph.core.matrix(axes = {"v": V}).cells[3].value, {"v": [1, 2]})

# The very object, not a copy.
L = [1]
def _grow(l):
    l.append(2)
heph.core.matrix(axes = {"l": [L]}, cell = _grow)
eq(L, [1, 2])
"#,
        )
        .expect("verbatim values");
    }

    /// Values are compared by Starlark equality: repeats are refused, and
    /// `exclude`, `include` and `default` match by it, lists included.
    #[test]
    fn matrix_values_compare_by_equality() {
        for (values, dup) in [
            ("[1, 1]", "1"),
            ("[[1], [1]]", "[1]"),
            ("[{\"a\": 1}, {\"a\": 1}]", "{\"a\": 1}"),
            ("[(1, \"x\"), (1, \"x\")]", "(1, \"x\")"),
            ("[1, 1.0]", "1.0"),
            ("[None, 2, None]", "None"),
            ("[\"a\", \"b\", \"a\"]", "\"a\""),
        ] {
            let err = eval_err(&format!("heph.core.matrix(axes = {{\"v\": {values}}})"));
            assert!(
                err.contains(&format!(
                    "heph.core.matrix() at p/BUILD:2: axis \"v\" lists {dup} twice"
                )),
                "{values}: {err}"
            );
        }
        eval(
            r#"
M = heph.core.matrix(
    axes = {"flags": [["-a"], ["-b"]], "n": [1, None]},
    cell = lambda flags, n: (flags, n),
    exclude = [{"flags": ["-a"], "n": None}],
    include = [{"n": 2, "flags": []}],
    default = {"n": None, "flags": ["-b"]},
)
eq([c.value for c in M.cells], [(["-a"], 1), (["-b"], 1), (["-b"], None), ([], 2)])
eq(M.default, (["-b"], None))
eq(heph.core.matrix(axes = {"d": [{"k": 1}, {"k": 2}]}, exclude = [{"d": {"k": 1}}]).cells[0].axes, {"d": {"k": 2}})
eq(heph.core.matrix(axes = {"n": [1]}, include = [{"n": 1.5}], default = {"n": 1.5}).default, {"n": 1.5})
eq(heph.core.matrix(axes = {"t": [(1, "a"), (2, "b")]}, default = {"t": (2, "b")}).default, {"t": (2, "b")})
"#,
        )
        .expect("matching by equality");
        // 1 == 1.0, so an include of 1.0 is the cell 1.
        let err = eval_err("heph.core.matrix(axes = {\"n\": [1]}, include = [{\"n\": 1.0}])");
        assert!(
            err.contains("include {n=1.0} is already a cell of this matrix"),
            "{err}"
        );
        // A list value naming no cell, shown by its repr.
        let err = eval_err(
            "heph.core.matrix(axes = {\"flags\": [[\"-a\"]], \"n\": [1]}, default = {\"flags\": [\"-b\"], \"n\": 1})",
        );
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2: default {flags=[\"-b\"], n=1} names no cell of \
                 this matrix. Its cells: {flags=[\"-a\"], n=1}. Add it with include, or name \
                 another cell"
            ),
            "{err}"
        );
        let err = eval_err(
            "heph.core.matrix(axes = {\"flags\": [[\"-a\"]]}, exclude = [{\"flags\": [\"-b\"]}])",
        );
        assert!(
            err.contains(
                "exclude {flags=[\"-b\"]} matches no cell: [\"-b\"] is not a value of axis \"flags\""
            ),
            "{err}"
        );
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
                "axis \"go\" must be a list of values, got string",
            ),
            (
                r#"heph.core.matrix(axes = {"go": 1})"#,
                "must be a list of values, got int",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ("1",)})"#,
                "must be a list of values, got tuple",
            ),
            (
                r#"heph.core.matrix(axes = {"go": {"1": "2"}})"#,
                "must be a list of values, got dict",
            ),
            (
                r#"heph.core.matrix(axes = {1: ["a"]})"#,
                "an axis name must be a string, got int",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1", "1"]})"#,
                "axis \"go\" lists \"1\" twice",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = [{"os": "x"}])"#,
                "exclude names axis \"os\", which is not an axis of this matrix (\"go\")",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = [{"go": 1}])"#,
                "exclude {go=1} matches no cell: 1 is not a value of axis \"go\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, exclude = {"go": "1"})"#,
                "`exclude` must be a list of dicts",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"], "r": ["a"]}, include = [{"go": "2"}])"#,
                "gives no value for axis \"r\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{"go": "2", "os": "x"}])"#,
                "names axis \"os\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{"go": "1"}])"#,
                "include {go=\"1\"} is already a cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, include = [{1: "2"}])"#,
                "each `include` entry must map axis names (strings) to values, got int",
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
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = "1")"#,
                "`default` must be None or a dict of axis name to value (got string)",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"go": 1})"#,
                "default {go=1} names no cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"os": "x", "go": "1"})"#,
                "default names axis \"os\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"], "r": ["a"]}, default = {"go": "1"})"#,
                "default gives no value for axis \"r\"",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = {"go": "2"})"#,
                "names no cell",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = 1)"#,
                "(got int)",
            ),
            (
                r#"heph.core.matrix(axes = {"go": ["1"]}, default = ["1"])"#,
                "(got list)",
            ),
        ];
        for (call, want) in cases {
            let err = eval_err(call);
            assert!(
                err.contains("heph.core.matrix() at p/BUILD:2") && err.contains(want),
                "{call}\nwant {want:?} at p/BUILD:2, got:\n{err}"
            );
        }

        // A cell whose parameters do not match the axes fails at the first
        // call, naming the call site and the cell, with starlark's own text
        // naming the parameter: one the axes do not give...
        let err = eval_err(
            "def _c(go, race):\n    return go\nheph.core.matrix(axes = {\"race\": [True]}, cell = _c)",
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:4: cell race=True")
                && err.contains(": Missing parameter `go` for call to `"),
            "{err}"
        );
        // ...and an axis the cell does not take.
        let err = eval_err(
            "def _c(go):\n    return go\nheph.core.matrix(axes = {\"go\": [\"1\"], \"race\": [True]}, cell = _c)",
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:4: cell go=\"1\", race=True")
                && err.contains(": Found `race` extra named parameter(s) for call to "),
            "{err}"
        );
    }

    /// T3.
    #[test]
    fn matrix_returns_cell_values_verbatim() {
        eval(
            r#"
V = {"a": "s", "b": ":rel", "c": {"bin": ":x"}, "d": [1, ":y"], "e": None}
M = heph.core.matrix(axes = {"k": ["a", "b", "c", "d", "e"]}, cell = lambda k: V[k])
eq([c.value for c in M.cells], V.values())
for k in V:
    eq(heph.core.matrix(axes = {"k": ["a", "b", "c", "d", "e"]}, cell = lambda k: V[k], default = {"k": k}).default, V[k])
"#,
        )
        .expect("verbatim");

        let err = eval_err(
            r#"def _cell(go, race):
    if race:
        fail("boom")
    return go
heph.core.matrix(axes = {"go": ["1.23"], "race": [False, True]}, cell = _cell)"#,
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:6: cell go=\"1.23\", race=True"),
            "{err}"
        );
        assert!(err.contains("boom"), "{err}");
        // The cell's own traceback is kept.
        assert!(err.contains("_cell"), "{err}");
    }

    /// T6.
    #[test]
    fn matrix_default() {
        eval(
            r#"
M = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "race": [True, False]},
    cell = lambda go, race: go + ("-race" if race else ""),
    default = {"race": False, "go": "1.23"},
)
eq(M.default, "1.23")
eq(hasattr(M, "default"), True)
eq(dir(M), ["cells", "default"])
F = heph.core.matrix(axes = {"go": ["1.22", "1.23"], "race": [True, False]}, default = {"race": False, "go": "1.22"})
eq(F.default, {"go": "1.22", "race": False})
"#,
        )
        .expect("default");

        // A default naming no cell fails at the call, not at a read.
        let err = eval_err(
            "M = heph.core.matrix(axes = {\"go\": [\"1.22\", \"1.23\"], \"race\": [True, False]}, default = {\"go\": \"1.24\", \"race\": True})",
        );
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2: default {go=\"1.24\", race=True} names no cell \
                 of this matrix. Its cells: {go=\"1.22\", race=True}, {go=\"1.22\", race=False}, \
                 {go=\"1.23\", race=True}, {go=\"1.23\", race=False}. Add it with include, or \
                 name another cell"
            ),
            "{err}"
        );

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
        )
        .expect("no default attribute");
        let err = eval_err(
            "M = heph.core.matrix(axes = {\"go\": [\"1\"]}, default = None)\nX = M.default",
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
M.cells[0] = "changed"
M.cells.pop()
eq(M.cells, ["changed"])
eq(M.default, "a!")
"#,
        )
        .expect("default kept");
    }

    /// T7.
    #[test]
    fn matrix_result_through_load() {
        let defs = r#"M = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "flags": [["-race"]]},
    cell = lambda go, flags: "//lib:t." + go,
    default = {"flags": ["-race"], "go": "1.23"},
)
N = heph.core.matrix(axes = {"go": ["1.22"]})

def make(name):
    return heph.core.matrix(
        axes = {"go": ["1.22", "1.23"]},
        cell = lambda go: target(name = name + "." + go, driver = "d"),
    )
"#;
        // Both packages read the frozen value: `cells`, `default`, and
        // `hasattr` on one without a default.
        let consumer = r#"load("//t/eq.star", "eq")
load("//lib/defs.star", "M", "N")
eq([c.value for c in M.cells], ["//lib:t.1.22", "//lib:t.1.23"])
eq(M.cells[1].axes, {"go": "1.23", "flags": ["-race"]})
eq(hasattr(M, "default"), True)
eq(hasattr(N, "default"), False)
eq(dir(N), ["cells"])
target(name = "use", driver = "d", tool = M.default, all = [c.value for c in M.cells])
"#;
        let files = [
            ("lib/defs.star", defs),
            ("a/BUILD", consumer),
            ("b/BUILD", consumer),
            (
                "c/BUILD",
                "load(\"//t/eq.star\", \"eq\")\nload(\"//lib/defs.star\", \"make\")\nT = make(\"tool\")\neq(T.cells[1].value, \"//c:tool.1.23\")\n",
            ),
            (
                "d/BUILD",
                "load(\"//lib/defs.star\", \"M\")\nM.cells.append(1)\n",
            ),
        ];
        let config = |pkg: &str| {
            let r = eval_pkg(&files, pkg).unwrap_or_else(|e| panic!("{pkg}: {e:#}"));
            r.target("use").expect("use").config.clone()
        };
        let (a, b) = (config("a"), config("b"));
        assert_eq!(
            a.get("tool"),
            Some(&hcore::htvalue::Value::String("//lib:t.1.23".to_string()))
        );
        assert_eq!(
            a.get("tool"),
            b.get("tool"),
            "`.default` is one value across packages"
        );
        assert_eq!(a.get("all"), b.get("all"));
        let c = eval_pkg(&files, "c").expect("a matrix in a def");
        let names: Vec<&str> = c.targets().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["tool.1.22", "tool.1.23"]);
        // A loaded result is frozen.
        let err = format!("{:#}", eval_pkg(&files, "d").unwrap_err());
        assert!(err.contains("Immutable"), "{err}");
    }

    /// T13.
    #[test]
    fn matrix_duplicate_names() {
        let err = eval_err(
            r#"def _c(go):
    return target(name = "tool", driver = "http_fetch", url = "https://x/" + go)
heph.core.matrix(axes = {"go": ["1.22", "1.23"]}, cell = _c)"#,
        );
        assert!(
            err.contains(
                "//p:tool is declared twice with different configs: \
                 p/BUILD:3 (cell go=\"1.22\") and p/BUILD:3 (cell go=\"1.23\"). \
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
        );
        assert!(
            err.contains("p/BUILD:2 and p/BUILD:3 (cell v=\"a\")"),
            "{err}"
        );

        // Names a cell builds that collide: `1.2` + `3` against `1` + `2.3`.
        let err = eval_err(
            r#"heph.core.matrix(axes = {"a": ["1.2", "1"], "b": ["3", "2.3"]}, cell = lambda a, b: target(name = "t." + a + "." + b, driver = "d", a = a, b = b))"#,
        );
        assert!(
            err.contains(
                "//p:t.1.2.3 is declared twice with different configs: p/BUILD:2 (cell a=\"1.2\", \
                 b=\"3\") and p/BUILD:2 (cell a=\"1\", b=\"2.3\"). They differ in config.a, config.b."
            ),
            "{err}"
        );

        // The same declaration from every cell collapses to one target.
        let result = eval(
            r#"heph.core.matrix(axes = {"go": ["1.22", "1.23"]}, cell = lambda go: target(name = "shared", driver = "d"))"#,
        )
        .expect("identical declarations");
        assert_eq!(result.targets().len(), 1);
    }

    /// T14.
    #[test]
    fn matrix_limits() {
        let err = eval_err(
            "heph.core.matrix(axes = {\"v\": list(range(1025))}, cell = lambda v: fail(\"ran\"))",
        );
        assert!(
            err.contains(
                "heph.core.matrix() at p/BUILD:2 would have 1,025 cells; the limit is 1,024"
            ) && !err.contains("fail: ran"),
            "{err}"
        );
        let err = eval_err(
            "heph.core.matrix(axes = {\"a%d\" % i: [\"x\", \"y\"] for i in range(40)}, cell = lambda **kw: fail(\"ran\"))",
        );
        assert!(
            err.contains("would have 1,099,511,627,776 cells; the limit is 1,024"),
            "{err}"
        );
        eval(
            "eq(len(heph.core.matrix(axes = {\"v\": list(range(1024))}, cell = lambda v: None).cells), 1024)",
        )
        .expect("exactly the limit");
        // A huge axis is refused by its length, before any value is compared
        // (the repeated last value is never reached), so the quadratic
        // duplicate check never runs over more than the limit.
        let err = eval_err("heph.core.matrix(axes = {\"v\": list(range(200000)) + [0]})");
        assert!(
            err.contains("would have 200,001 cells; the limit is 1,024"),
            "{err}"
        );
        // `include` counts before the axes are validated, too.
        let err = eval_err(
            "heph.core.matrix(axes = {\"v\": list(range(1024))}, include = [{\"v\": \"x\"}])",
        );
        assert!(err.contains("would have 1,025 cells"), "{err}");

        let err = eval_err(
            "heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = lambda a: heph.core.matrix(axes = {\"b\": [\"2\"]}))",
        );
        assert!(
            err.contains("heph.core.matrix() at p/BUILD:2: a cell may not call heph.core.matrix()"),
            "{err}"
        );
        // A cell passing itself ends in the same error, not a stack overflow.
        let err = eval_err(
            "def _c(a):\n    return heph.core.matrix(axes = {\"a\": [\"1\"]}, cell = _c)\n_c(\"x\")",
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
