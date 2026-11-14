//! Reading a cached report without paying for its tables.
//!
//! A three-hour session's `<id>.report.json` is ~60 MB, and all but ~30 KB of
//! it is four row tables: `flicks.flicks`, `clicks.clicks`, `per_second` and
//! `per_minute`. A trend row and `report --summary` read none of them, and
//! used to deserialize all of them (`docs/PERFORMANCE-2026-11-14.md`, N5).
//!
//! Two partial readers over the same document, neither a schema of its own:
//!
//! * [`Stamps`] — the provenance fields a cache is judged by. They are the
//!   first two kilobytes of the document, so a cache that is about to be
//!   refused (another analyzer version, other parameters) is refused before
//!   anything else is parsed.
//! * [`Headlines`] — every section except the tables, as a [`Report`] whose
//!   tables are empty. It stays inside this crate: the only things made from
//!   it are the two projections that never look at a table,
//!   [`Report::headline`] and [`crate::trend::row`].
//!
//! Both stop reading as soon as they have every field they want. The report
//! is written with the tables last, so the `per_second` rows (two thirds of
//! the file) are never even scanned; a document with its keys in another
//! order (hand-edited, re-serialized by another tool) is read to the end
//! instead and gives the same answer.

use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};

use super::{Report, SessionSummary};
use crate::clicks::{ButtonStats, ClickReport};
use crate::flicks::{FlickParams, FlickReport};
use crate::kinematics::Kinematics;
use crate::lifts::LiftReport;
use crate::load::FileSignature;
use crate::markers::{MarkerRow, SegmentReport};
use crate::micro::MicroReport;
use crate::quality::QualityReport;
use crate::series::Params;
use crate::stats::Summary;

/// A chosen set of the report document's top-level fields, filled one key at
/// a time.
pub(crate) trait Partial: Default {
    /// The fields, once all of them have been read.
    type Done;
    /// Read `key`'s value if it is one of ours; `false` leaves the value for
    /// the caller to skip.
    fn take<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error>;
    /// Every field has been read.
    fn complete(&self) -> bool;
    fn finish(self) -> Option<Self::Done>;
}

/// Declares the finished struct, and the all-`Option` slots that implement
/// [`Partial`] for it. The field names are the document's keys.
macro_rules! partial {
    (
        $(#[$meta:meta])*
        struct $name:ident via $slots:ident {
            $($field:ident: $ty:ty,)*
        }
    ) => {
        $(#[$meta])*
        pub(crate) struct $name {
            $(pub(crate) $field: $ty,)*
        }

        #[derive(Default)]
        pub(crate) struct $slots {
            $($field: Option<$ty>,)*
        }

        impl Partial for $slots {
            type Done = $name;

            fn take<'de, A: MapAccess<'de>>(
                &mut self,
                key: &str,
                map: &mut A,
            ) -> Result<bool, A::Error> {
                match key {
                    $(stringify!($field) => self.$field = Some(map.next_value()?),)*
                    _ => return Ok(false),
                }
                Ok(true)
            }

            fn complete(&self) -> bool {
                true $(&& self.$field.is_some())*
            }

            fn finish(self) -> Option<$name> {
                Some($name {
                    $($field: self.$field?,)*
                })
            }
        }
    };
}

/// Fills a [`Partial`] from the document's top-level object, returning the
/// moment it is complete.
struct Fill<'a, T>(&'a mut T);

impl<'de, T: Partial> DeserializeSeed<'de> for Fill<'_, T> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, T: Partial> Visitor<'de> for Fill<'_, T> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a report object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if !self.0.take(&key, &mut map)? {
                map.next_value::<IgnoredAny>()?;
            }
            if self.0.complete() {
                break;
            }
        }
        Ok(())
    }
}

/// The fields `T` names, out of the JSON report in `bytes`; `None` when one
/// of them is missing or anything read on the way to them does not parse.
///
/// Reading stops at the last wanted field, so what follows it is not looked
/// at — not even for being JSON. That is the point (it is the tables), and it
/// is sound for a cache: the answer is made only of what was read, and the
/// file is published by an atomic rename, so there is no half-written tail to
/// guard against.
pub(crate) fn read_partial<T: Partial>(bytes: &[u8]) -> Option<T::Done> {
    let mut slots = T::default();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    // Stopping early leaves the object open, which serde_json reports as an
    // error once the visitor returns. Completeness is the verdict, not that.
    let _ = Fill(&mut slots).deserialize(&mut de);
    slots.finish()
}

partial! {
    /// What a cached report is judged by before anything else is read.
    struct Stamps via StampSlots {
        schema: String,
        analyzer_version: String,
        recording_signature: Option<FileSignature>,
        params: Params,
        session: SessionSummary,
    }
}

partial! {
    /// Every top-level field of [`Report`] ahead of its tables.
    struct Sections via SectionSlots {
        schema: String,
        analyzer_version: String,
        generated_utc_us: i64,
        compute_ms: f64,
        load_ms: f64,
        total_ms: f64,
        recording_signature: Option<FileSignature>,
        grid_cells: usize,
        grid_stored_cells: usize,
        grid_dt_us: i64,
        params: Params,
        session: SessionSummary,
        quality: QualityReport,
        kinematics: Kinematics,
        flicks: FlickHead,
        micro: MicroReport,
        clicks: ClickHead,
        lifts: LiftHead,
        markers: Vec<MarkerRow>,
        segments: Vec<SegmentReport>,
    }
}

/// [`FlickReport`] without its per-flick rows. Converting back names every
/// field, so one added to `FlickReport` does not compile until it is carried
/// here too.
#[derive(Deserialize)]
pub(crate) struct FlickHead {
    params: FlickParams,
    count: usize,
    per_minute: f64,
    amplitude_deg: Summary,
    peak_velocity_deg_s: Summary,
    duration_ms: Summary,
    overshoot_ratio: Summary,
    settle_ms: Summary,
    time_to_click_ms: Summary,
    clicked_fraction: f64,
}

impl From<FlickHead> for FlickReport {
    fn from(h: FlickHead) -> Self {
        Self {
            params: h.params,
            count: h.count,
            per_minute: h.per_minute,
            amplitude_deg: h.amplitude_deg,
            peak_velocity_deg_s: h.peak_velocity_deg_s,
            duration_ms: h.duration_ms,
            overshoot_ratio: h.overshoot_ratio,
            settle_ms: h.settle_ms,
            time_to_click_ms: h.time_to_click_ms,
            clicked_fraction: h.clicked_fraction,
            flicks: Vec::new(),
        }
    }
}

/// [`ClickReport`] without its per-click rows.
#[derive(Deserialize)]
pub(crate) struct ClickHead {
    total_clicks: usize,
    clicks_per_min: f64,
    pre_click_speed_counts_s: Summary,
    pre_click_speed_cm_s: Summary,
    still_click_fraction: f64,
    click_to_still_ms: Summary,
    hold_ms: Summary,
    double_click_interval_ms: Summary,
    double_clicks: usize,
    unmatched_downs: usize,
    per_button: Vec<ButtonStats>,
}

impl From<ClickHead> for ClickReport {
    fn from(h: ClickHead) -> Self {
        Self {
            total_clicks: h.total_clicks,
            clicks_per_min: h.clicks_per_min,
            pre_click_speed_counts_s: h.pre_click_speed_counts_s,
            pre_click_speed_cm_s: h.pre_click_speed_cm_s,
            still_click_fraction: h.still_click_fraction,
            click_to_still_ms: h.click_to_still_ms,
            hold_ms: h.hold_ms,
            double_click_interval_ms: h.double_click_interval_ms,
            double_clicks: h.double_clicks,
            unmatched_downs: h.unmatched_downs,
            per_button: h.per_button,
            clicks: Vec::new(),
        }
    }
}

/// [`LiftReport`] without its per-lift rows.
#[derive(Deserialize)]
pub(crate) struct LiftHead {
    count: usize,
    per_minute: f64,
    mean_drift_cm: f64,
}

impl From<LiftHead> for LiftReport {
    fn from(h: LiftHead) -> Self {
        Self {
            count: h.count,
            per_minute: h.per_minute,
            mean_drift_cm: h.mean_drift_cm,
            lifts: Vec::new(),
        }
    }
}

/// A cached report with every row table left on disk: `flicks.flicks`,
/// `clicks.clicks`, `lifts.lifts`, `per_second`, `per_minute` and `timings`
/// are empty, and `warnings` is empty until the sidecar is refreshed (a cache
/// hit recomputes it from the quality section anyway).
///
/// Deliberately not a [`Report`] to the outside: the wrapper only hands out
/// the projections that are the same with or without the tables.
pub(crate) struct Headlines(Report);

impl Headlines {
    /// The table-free sections of the JSON report in `bytes`.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        let s = read_partial::<SectionSlots>(bytes)?;
        Some(Self(Report {
            schema: s.schema,
            analyzer_version: s.analyzer_version,
            generated_utc_us: s.generated_utc_us,
            compute_ms: s.compute_ms,
            load_ms: s.load_ms,
            total_ms: s.total_ms,
            recording_signature: s.recording_signature,
            grid_cells: s.grid_cells,
            grid_stored_cells: s.grid_stored_cells,
            grid_dt_us: s.grid_dt_us,
            params: s.params,
            session: s.session,
            quality: s.quality,
            kinematics: s.kinematics,
            flicks: s.flicks.into(),
            micro: s.micro,
            clicks: s.clicks.into(),
            lifts: s.lifts.into(),
            markers: s.markers,
            segments: s.segments,
            per_second: Vec::new(),
            per_minute: Vec::new(),
            warnings: Vec::new(),
            timings: Vec::new(),
        }))
    }

    /// `report --summary` for this report, with the sidecar-derived fields
    /// brought up to date first — what [`Report::refresh_sidecar`] then
    /// [`Report::headline`] give on the full document.
    pub(crate) fn summary(
        mut self,
        meta: Option<&telemouse_core::recordings::SessionMeta>,
    ) -> super::ReportSummary {
        self.0.refresh_sidecar(meta);
        self.0.headline()
    }

    /// The trend row for this report, read from the cache. No `--metric`
    /// columns: a dotted path may point into a table.
    pub(crate) fn trend_row(&self) -> crate::trend::TrendRow {
        crate::trend::row(&self.0, true, &[])
    }
}
