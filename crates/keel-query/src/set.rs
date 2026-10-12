//! Functions over the whole result set: `dupe:` (with `dupe-count:`, `dupe-from:`,
//! `dupe-min:`, `dupe-max:` and Everything 1.4's `sizedupe:` family), `distinct:`,
//! `distinct-sort:`, `unique:` and `count:`. They are taken out of the query before it is
//! compiled (so they may only be ANDed at the top, not inside `|` or `!`), every item that
//! matches the rest is pushed into a [`SetPass`], and [`SetPass::finish`] gives the result.
//!
//! Memory is bounded: a pass keeps one row (24 bytes) and the encoded property values of each
//! candidate, and refuses more than [`Options::set_memory_limit`](crate::Options) bytes
//! with an [`ErrorKind::Limit`] error instead of growing.

use std::ops::Range;
use std::sync::Arc;

use crate::eval::{num_test, push_key, settings, trim_sep, value_err, Ctx, Env, Key};
use crate::functions::{FnKind, SetFn};
use crate::parse::{Expr, Item, Term};
use crate::props::{normalize, Props};
use crate::text::Fold;
use crate::tree::Tree;
use crate::values::{parse_number, NumTest};
use crate::{Error, ErrorKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Dupe,
    Unique,
    Distinct { sort: bool },
}

/// What to do with the matched items.
pub(crate) struct SetSpec {
    group: Option<Group>,
    count: Option<u64>,
    span: Range<usize>,
}

struct Group {
    mode: Mode,
    keys: Vec<Key>,
    min: u64,
    max: u64,
    count: Option<NumTest>,
    /// Folded folder paths without trailing separators.
    from: Vec<(String, Fold)>,
}

/// Takes the set functions out of `e`: they may be ANDed at the top level (in groups too),
/// never under `|` or `!`.
pub(crate) fn take(e: &mut Expr, top: bool, out: &mut Vec<Term>) -> Result<(), Error> {
    match e {
        Expr::Term(t) => {
            if let Some(f) = &t.function {
                if let FnKind::Set(_) = f.kind {
                    if !top {
                        return Err(Error {
                            span: t.span.clone(),
                            kind: ErrorKind::Syntax,
                            message: format!(
                                "`{}:` works on the whole result; it cannot be inside `|` or `!`",
                                f.name
                            ),
                        });
                    }
                    out.push(t.clone());
                    // `file:dupe:size` keeps its `file:`/`folder:` as a filter.
                    *e = Expr::Term(Term {
                        span: t.span.clone(),
                        modifiers: t.modifiers.clone(),
                        function: None,
                        items: Vec::new(),
                    });
                }
            }
            Ok(())
        }
        Expr::And(v) => v.iter_mut().try_for_each(|x| take(x, top, out)),
        Expr::Or(v) => v.iter_mut().try_for_each(|x| take(x, false, out)),
        Expr::Not(x) => take(x, false, out),
    }
}

fn term_err(t: &Term, message: impl Into<String>) -> Error {
    Error {
        span: t.span.clone(),
        kind: ErrorKind::Value,
        message: message.into(),
    }
}

/// The single value of a term.
fn one(t: &Term) -> Result<&Item, Error> {
    match t.items.as_slice() {
        [item] => Ok(item),
        _ => Err(term_err(t, "this function takes one value")),
    }
}

fn int(item: &Item) -> Result<u64, Error> {
    let t = item.text.trim();
    t.parse::<u64>()
        .map_err(|_| value_err(item, format!("`{t}` is not a whole number")))
}

/// Compiles the set terms of a query.
pub(crate) fn compile(terms: &[Term], env: &Env) -> Result<Option<SetSpec>, Error> {
    if terms.is_empty() {
        return Ok(None);
    }
    let span = terms[0].span.start..terms[terms.len() - 1].span.end;
    let mut group: Option<Group> = None;
    let mut implied = false;
    let mut count = None;
    let mut min = 2;
    let mut max = u64::MAX;
    let mut dupe_count = None;
    let mut from = Vec::new();
    let once =
        |t: &Term, what: &str| term_err(t, format!("{what} can only be used once in a search"));
    for t in terms {
        let Some(FnKind::Set(f)) = t.function.as_ref().map(|f| f.kind) else {
            continue;
        };
        let s = settings(&t.modifiers, &env.opts);
        let mode = match f {
            SetFn::Dupe | SetFn::Legacy(_) => Some(Mode::Dupe),
            SetFn::Unique => Some(Mode::Unique),
            SetFn::Distinct => Some(Mode::Distinct { sort: false }),
            SetFn::DistinctSort => Some(Mode::Distinct { sort: true }),
            _ => None,
        };
        if let Some(mode) = mode {
            if group.is_some() && !implied {
                return Err(once(t, "`dupe:`, `unique:` or `distinct:`"));
            }
            let names: Vec<(String, &Item)> = match f {
                SetFn::Legacy(_) => {
                    if let Some(item) = t.items.first() {
                        return Err(value_err(item, "this function takes no value".into()));
                    }
                    Vec::new()
                }
                _ => t.items.iter().map(|i| (i.text.clone(), i)).collect(),
            };
            let mut keys = Vec::new();
            for (name, item) in &names {
                if item.op.is_some() {
                    return Err(value_err(item, "a property list takes no operators".into()));
                }
                keys.push(Key::of(name, s.fold).map_err(|m| value_err(item, m))?);
            }
            if keys.is_empty() {
                let name = match f {
                    SetFn::Legacy(prop) => prop,
                    _ => "name",
                };
                keys.push(Key::of(name, s.fold).map_err(|m| term_err(t, m))?);
            }
            group = Some(Group {
                mode,
                keys,
                min: 0,
                max: 0,
                count: None,
                from: Vec::new(),
            });
            implied = false;
            continue;
        }
        match f {
            SetFn::Count => {
                if count.is_some() {
                    return Err(once(t, "`count:`"));
                }
                count = Some(int(one(t)?)?);
            }
            SetFn::DupeMin => min = int(one(t)?)?,
            SetFn::DupeMax => max = int(one(t)?)?,
            SetFn::DupeCount => dupe_count = Some(num_test(one(t)?, &parse_number)?),
            SetFn::DupeFrom => {
                if t.items.is_empty() {
                    return Err(term_err(t, "dupe-from: needs a folder"));
                }
                let fold = Fold {
                    diacritics: true,
                    ..s.fold
                };
                for item in &t.items {
                    let prefix = fold.apply(trim_sep(&normalize(&item.text))).into_owned();
                    if prefix.is_empty() {
                        return Err(value_err(item, "dupe-from: needs a folder".into()));
                    }
                    from.push((prefix, fold));
                }
            }
            _ => continue,
        }
        // `dupe-min:` and friends alone mean `dupe:name`.
        if group.is_none() && f != SetFn::Count {
            group = Some(Group {
                mode: Mode::Dupe,
                keys: vec![Key::of("name", s.fold).map_err(|m| term_err(t, m))?],
                min: 0,
                max: 0,
                count: None,
                from: Vec::new(),
            });
            implied = true;
        }
    }
    if let Some(g) = &mut group {
        if g.mode != Mode::Dupe
            && (min != 2 || max != u64::MAX || dupe_count.is_some() || !from.is_empty())
        {
            return Err(Error {
                span: span.clone(),
                kind: ErrorKind::Value,
                message: "`dupe-count:`, `dupe-from:`, `dupe-min:` and `dupe-max:` go with `dupe:`"
                    .into(),
            });
        }
        g.min = min.max(2);
        g.max = max;
        g.count = dupe_count;
        g.from = from;
    }
    Ok(Some(SetSpec { group, count, span }))
}

/// One candidate: the caller's handle and where its key is.
struct Row {
    handle: u64,
    start: usize,
    len: u32,
    from: bool,
}

/// The set functions of a query, applied to the items that match the rest of it. Push every
/// matching item (in result order), then [`finish`](SetPass::finish).
pub struct SetPass {
    spec: Arc<SetSpec>,
    env: Arc<Env>,
    rows: Vec<Row>,
    keys: Vec<u8>,
}

impl SetPass {
    pub(crate) fn new(spec: Arc<SetSpec>, env: Arc<Env>) -> Self {
        Self {
            spec,
            env,
            rows: Vec::new(),
            keys: Vec::new(),
        }
    }

    /// Adds a matching item, known to the caller by `handle`. `Ok(false)`: nothing more is
    /// needed (`count:` is reached), stop pushing. Errors when the pass would use more than
    /// [`Options::set_memory_limit`](crate::Options) bytes.
    pub fn push(
        &mut self,
        handle: u64,
        item: &dyn Props,
        tree: Option<&dyn Tree>,
    ) -> Result<bool, Error> {
        let Some(g) = &self.spec.group else {
            if self.spec.count.is_some_and(|c| self.rows.len() as u64 >= c) {
                return Ok(false);
            }
            self.rows.push(Row {
                handle,
                start: 0,
                len: 0,
                from: false,
            });
            return Ok(self.spec.count.is_none_or(|c| (self.rows.len() as u64) < c));
        };
        let start = self.keys.len();
        let mut ctx = Ctx::new(tree, self.env.opts.tree_scan_limit);
        if !push_key(&g.keys, item, &self.env, &mut ctx, &mut self.keys) {
            // Unknown values are never duplicates, distinct or unique.
            return Ok(true);
        }
        let len = u32::try_from(self.keys.len() - start).unwrap_or(u32::MAX);
        let from = !g.from.is_empty() && {
            let path = normalize(item.path());
            g.from.iter().any(|(prefix, fold)| {
                let p = fold.apply(&path);
                p.len() > prefix.len()
                    && p.starts_with(prefix.as_str())
                    && p[prefix.len()..].starts_with('\\')
            })
        };
        self.rows.push(Row {
            handle,
            start,
            len,
            from,
        });
        let used = self.keys.len() + self.rows.len() * std::mem::size_of::<Row>();
        if used > self.env.opts.set_memory_limit || self.rows.len() >= u32::MAX as usize {
            return Err(Error {
                span: self.spec.span.clone(),
                kind: ErrorKind::Limit,
                message: format!(
                    "too many results for this function ({} so far, {} MB); narrow the search",
                    self.rows.len(),
                    used >> 20
                ),
            });
        }
        Ok(true)
    }

    /// The handles of the result, in order: duplicate groups sorted by their value (items
    /// of a group in the order they were pushed), unique and distinct results in pushed
    /// order (`distinct-sort:` by value), then cut to `count:`.
    pub fn finish(self) -> Vec<u64> {
        let SetPass {
            spec, rows, keys, ..
        } = self;
        let limit = spec
            .count
            .map_or(usize::MAX, |c| usize::try_from(c).unwrap_or(usize::MAX));
        let Some(g) = &spec.group else {
            return rows.iter().take(limit).map(|r| r.handle).collect();
        };
        let key = |i: u32| {
            let r = &rows[i as usize];
            &keys[r.start..r.start + r.len as usize]
        };
        let mut order: Vec<u32> = (0..rows.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| key(a).cmp(key(b)).then(a.cmp(&b)));
        let mut picked: Vec<u32> = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let mut j = i + 1;
            while j < order.len() && key(order[j]) == key(order[i]) {
                j += 1;
            }
            let run = &order[i..j];
            let n = run.len() as u64;
            match g.mode {
                Mode::Dupe => {
                    let keep = n >= g.min
                        && n <= g.max
                        && g.count.is_none_or(|t| t.eval(Some(n as f64)))
                        && (g.from.is_empty() || run.iter().any(|&r| rows[r as usize].from));
                    if keep {
                        picked.extend_from_slice(run);
                    }
                }
                Mode::Unique => {
                    if n == 1 {
                        picked.push(run[0]);
                    }
                }
                Mode::Distinct { .. } => picked.push(run[0]),
            }
            i = j;
        }
        if matches!(g.mode, Mode::Unique | Mode::Distinct { sort: false }) {
            picked.sort_unstable();
        }
        picked
            .into_iter()
            .take(limit)
            .map(|r| rows[r as usize].handle)
            .collect()
    }
}
