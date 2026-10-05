//! Instruments a binding declares (design §4.2): counters, up-down counters,
//! histograms and gauges by `u64` handle, plus interned attribute sets, so a
//! binding records through one cheap call (the C ABI's `aster_instrument_*`,
//! `aster_attrset_*`, `aster_metric_*`).
//!
//! An instrument handle maps to its description plus the SDK instrument built
//! from the current global meter (scope `aster.app`) for the current pipeline
//! [`generation`](crate::generation): a record compares the generation (one
//! relaxed load) and rebuilds on a mismatch, so a handle taken before `init`,
//! or kept across a re-init, records into the current provider. Without a
//! pipeline the global meter is a no-op.
//!
//! Steady state (instrument built, attribute set interned, series seen
//! before): no syscalls, no I/O, short read locks only. A new series takes the
//! SDK's write lock once.
//!
//! The pipeline's teardown (`shutdown`, last handle dropped) frees every
//! instrument and attribute set handle (§7.1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use opentelemetry::metrics::{Counter, Gauge, Histogram, UpDownCounter};
use opentelemetry::{global, InstrumentationScope, KeyValue};

use crate::error::{Error, Result};

/// The instrumentation scope of binding-declared instruments and spans.
pub const APP_SCOPE_NAME: &str = "aster.app";

/// What an instrument handle records. Numbered as the C ABI's
/// `aster_instrument_kind_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum InstrumentKind {
    /// Monotonic `u64` sum.
    CounterU64 = 1,
    /// `i64` sum that may go down.
    UpDownI64 = 2,
    /// `f64` distribution.
    HistogramF64 = 3,
    /// `f64` last value.
    GaugeF64 = 4,
}

impl InstrumentKind {
    /// From the C ABI value.
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => InstrumentKind::CounterU64,
            2 => InstrumentKind::UpDownI64,
            3 => InstrumentKind::HistogramF64,
            4 => InstrumentKind::GaugeF64,
            _ => return None,
        })
    }
}

/// An instrument to declare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentDesc {
    pub kind: InstrumentKind,
    /// OTel instrument name: an ASCII letter, then letters, digits, `_`, `.`,
    /// `-`, `/`; at most 255 characters.
    pub name: String,
    pub description: String,
    /// UCUM unit (ASCII, at most 63 characters); empty for none.
    pub unit: String,
    /// Attribute keys merged in from the current call's extractor cache.
    /// Accepted and kept; merging lands with the per-call extractor cache
    /// (design §4.2, O6+): until then a record's `call_id` adds nothing.
    pub auto_attrs: Vec<String>,
    /// Series limit for this instrument; 0 = the SDK default (2000). Accepted
    /// and kept; the SDK's per-instrument limit needs a view at provider build
    /// time (`spec_unstable_metrics_views`), so every instrument gets the
    /// SDK default for now.
    pub cardinality_limit: u32,
}

/// A value to record. Each instrument kind takes exactly one variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Measurement {
    /// [`InstrumentKind::CounterU64`].
    U64(u64),
    /// [`InstrumentKind::UpDownI64`].
    I64(i64),
    /// [`InstrumentKind::HistogramF64`] and [`InstrumentKind::GaugeF64`].
    F64(f64),
}

enum Built {
    Counter(Counter<u64>),
    UpDown(UpDownCounter<i64>),
    Histogram(Histogram<f64>),
    Gauge(Gauge<f64>),
}

struct Instrument {
    desc: InstrumentDesc,
    /// The SDK instrument for a provider generation.
    built: RwLock<Option<(u64, Arc<Built>)>>,
}

impl Instrument {
    fn current(&self) -> Arc<Built> {
        let generation = crate::generation();
        if let Some((g, b)) = &*self.built.read().unwrap_or_else(|e| e.into_inner()) {
            if *g == generation {
                return b.clone();
            }
        }
        let b = Arc::new(self.build());
        *self.built.write().unwrap_or_else(|e| e.into_inner()) = Some((generation, b.clone()));
        b
    }

    fn build(&self) -> Built {
        let meter = global::meter_with_scope(app_scope());
        let d = &self.desc;
        let name = d.name.clone();
        macro_rules! common {
            ($b:expr) => {{
                let mut b = $b;
                if !d.description.is_empty() {
                    b = b.with_description(d.description.clone());
                }
                if !d.unit.is_empty() {
                    b = b.with_unit(d.unit.clone());
                }
                b.build()
            }};
        }
        match d.kind {
            InstrumentKind::CounterU64 => Built::Counter(common!(meter.u64_counter(name))),
            InstrumentKind::UpDownI64 => Built::UpDown(common!(meter.i64_up_down_counter(name))),
            InstrumentKind::HistogramF64 => Built::Histogram(common!(meter.f64_histogram(name))),
            InstrumentKind::GaugeF64 => Built::Gauge(common!(meter.f64_gauge(name))),
        }
    }
}

pub(crate) fn app_scope() -> InstrumentationScope {
    InstrumentationScope::builder(APP_SCOPE_NAME)
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

#[derive(Default)]
struct Table {
    instruments: HashMap<u64, Arc<Instrument>>,
    /// Live handles per name, with the name's kind (one kind per name).
    names: HashMap<String, (InstrumentKind, usize)>,
    attrsets: HashMap<u64, Arc<[KeyValue]>>,
}

static TABLE: RwLock<Option<Table>> = RwLock::new(None);
static NEXT: AtomicU64 = AtomicU64::new(1);
/// Serialises declarations (the name check and the insert).
static DECLARE: Mutex<()> = Mutex::new(());

fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    let mut g = TABLE.write().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Table::default))
}

fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 255
        && b[0].is_ascii_alphabetic()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'/'))
}

/// Declare an instrument; returns its handle (never 0, never reused).
/// [`Error::InvalidArgument`] for a name that breaks the OTel rules, a unit
/// that is not ASCII or longer than 63 characters, or a name already declared
/// with a different kind.
pub fn instrument_new(desc: InstrumentDesc) -> Result<u64> {
    if !valid_name(&desc.name) {
        return Err(Error::InvalidArgument(format!(
            "instrument name {:?} breaks the OTel naming rules (ASCII letter first, then \
             letters, digits, '_', '.', '-', '/'; at most 255)",
            desc.name
        )));
    }
    if !desc.unit.is_ascii() || desc.unit.len() > 63 {
        return Err(Error::InvalidArgument(
            "instrument unit must be ASCII and at most 63 characters".into(),
        ));
    }
    let _serial = DECLARE.lock().unwrap_or_else(|e| e.into_inner());
    with_table(|t| {
        match t.names.get_mut(&desc.name) {
            Some((kind, n)) if *kind == desc.kind => *n += 1,
            Some((kind, _)) => {
                return Err(Error::InvalidArgument(format!(
                    "instrument {:?} already exists as {:?}",
                    desc.name, kind
                )))
            }
            None => {
                t.names.insert(desc.name.clone(), (desc.kind, 1));
            }
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        t.instruments.insert(
            id,
            Arc::new(Instrument {
                desc,
                built: RwLock::new(None),
            }),
        );
        Ok(id)
    })
}

/// Free an instrument handle. Returns whether it existed.
pub fn instrument_free(handle: u64) -> bool {
    with_table(|t| {
        let Some(i) = t.instruments.remove(&handle) else {
            return false;
        };
        if let Some((_, n)) = t.names.get_mut(&i.desc.name) {
            *n -= 1;
            if *n == 0 {
                t.names.remove(&i.desc.name);
            }
        }
        true
    })
}

/// The description behind `handle`.
pub fn instrument_desc(handle: u64) -> Option<InstrumentDesc> {
    let g = TABLE.read().unwrap_or_else(|e| e.into_inner());
    g.as_ref()?.instruments.get(&handle).map(|i| i.desc.clone())
}

/// Intern an attribute set (string values); returns its handle (never 0).
pub fn attrset_new(pairs: Vec<(String, String)>) -> Result<u64> {
    let set: Arc<[KeyValue]> = pairs
        .into_iter()
        .map(|(k, v)| KeyValue::new(k, v))
        .collect::<Vec<_>>()
        .into();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    with_table(|t| t.attrsets.insert(id, set));
    Ok(id)
}

/// Free an attribute set handle. Returns whether it existed.
pub fn attrset_free(handle: u64) -> bool {
    with_table(|t| t.attrsets.remove(&handle).is_some())
}

/// Record `value` on `instrument` with the attributes of `attrset` (0 = none)
/// followed by `extra`. [`Error::NotFound`] for an unknown instrument or
/// attribute set; [`Error::InvalidArgument`] when the value's type does not
/// match the instrument's kind, or a counter gets a negative-going value.
/// `call_id` is reserved for `auto_attrs` (see [`InstrumentDesc::auto_attrs`]).
pub fn record(
    instrument: u64,
    value: Measurement,
    attrset: u64,
    extra: &[(String, String)],
    call_id: u64,
) -> Result<()> {
    let _ = call_id;
    let (inst, set) = {
        let g = TABLE.read().unwrap_or_else(|e| e.into_inner());
        let t = g.as_ref().ok_or(Error::NotFound("instrument"))?;
        let inst = t
            .instruments
            .get(&instrument)
            .cloned()
            .ok_or(Error::NotFound("instrument"))?;
        let set = match attrset {
            0 => None,
            h => Some(
                t.attrsets
                    .get(&h)
                    .cloned()
                    .ok_or(Error::NotFound("attribute set"))?,
            ),
        };
        (inst, set)
    };
    let mismatch = || {
        Error::InvalidArgument(format!(
            "instrument {:?} is a {:?}",
            inst.desc.name, inst.desc.kind
        ))
    };
    match (inst.desc.kind, value) {
        (InstrumentKind::CounterU64, Measurement::U64(_))
        | (InstrumentKind::UpDownI64, Measurement::I64(_))
        | (InstrumentKind::HistogramF64 | InstrumentKind::GaugeF64, Measurement::F64(_)) => {}
        _ => return Err(mismatch()),
    }
    if crate::fork::forked() {
        return Ok(());
    }
    let owned: Vec<KeyValue>;
    let attrs: &[KeyValue] = match (&set, extra.is_empty()) {
        (Some(s), true) => s,
        (None, true) => &[],
        (s, false) => {
            owned = s
                .iter()
                .flat_map(|s| s.iter().cloned())
                .chain(
                    extra
                        .iter()
                        .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
                )
                .collect();
            &owned
        }
    };
    let built = inst.current();
    match (&*built, value) {
        (Built::Counter(c), Measurement::U64(v)) => c.add(v, attrs),
        (Built::UpDown(c), Measurement::I64(v)) => c.add(v, attrs),
        (Built::Histogram(h), Measurement::F64(v)) => h.record(v, attrs),
        (Built::Gauge(g), Measurement::F64(v)) => g.record(v, attrs),
        _ => return Err(mismatch()),
    }
    Ok(())
}

/// Live instrument and attribute set handles (tests, diagnostics).
pub fn live_handles() -> (usize, usize) {
    let g = TABLE.read().unwrap_or_else(|e| e.into_inner());
    g.as_ref()
        .map_or((0, 0), |t| (t.instruments.len(), t.attrsets.len()))
}

/// Free every handle (pipeline teardown, §7.1).
pub(crate) fn clear() {
    let old = TABLE.write().unwrap_or_else(|e| e.into_inner()).take();
    drop(old);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("app.requests_total"));
        assert!(valid_name("a/b-c"));
        assert!(!valid_name(""));
        assert!(!valid_name("1abc"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(&"a".repeat(256)));
    }
}
