//! Wire-only data transfer objects for schema-v1 metrics snapshots.
//!
//! These wire DTOs keep the daemon's runtime state out of consumer
//! dependency graphs. Serde is the crate's only production dependency,
//! and architecture tests enforce that boundary.

use std::collections::BTreeMap;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub schema: u32,
    pub metrics: Vec<MetricFamily>,
}

impl MetricsSnapshot {
    pub fn new(metrics: Vec<MetricFamily>) -> Self {
        Self { schema: 1, metrics }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MetricFamily {
    Counter {
        name: String,
        help: String,
        series: Vec<ValueSeries>,
    },
    Gauge {
        name: String,
        help: String,
        series: Vec<ValueSeries>,
    },
    Histogram {
        name: String,
        help: String,
        series: Vec<HistogramSeries>,
    },
}

// Keep the existing raw schema-v1 member order (`name`, `type`, `help`,
// `series`). Serde's internally tagged derive writes the tag before
// variant fields, so this explicit serializer preserves the raw stats
// byte shape.
impl Serialize for MetricFamily {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut family = serializer.serialize_struct("MetricFamily", 4)?;
        family.serialize_field("name", self.name())?;
        match self {
            Self::Counter { help, series, .. } => {
                family.serialize_field("type", "counter")?;
                family.serialize_field("help", help)?;
                family.serialize_field("series", series)?;
            }
            Self::Gauge { help, series, .. } => {
                family.serialize_field("type", "gauge")?;
                family.serialize_field("help", help)?;
                family.serialize_field("series", series)?;
            }
            Self::Histogram { help, series, .. } => {
                family.serialize_field("type", "histogram")?;
                family.serialize_field("help", help)?;
                family.serialize_field("series", series)?;
            }
        }
        family.end()
    }
}

impl MetricFamily {
    pub fn counter(name: String, help: String, series: Vec<ValueSeries>) -> Self {
        Self::Counter { name, help, series }
    }

    pub fn gauge(name: String, help: String, series: Vec<ValueSeries>) -> Self {
        Self::Gauge { name, help, series }
    }

    pub fn histogram(name: String, help: String, series: Vec<HistogramSeries>) -> Self {
        Self::Histogram { name, help, series }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Counter { name, .. }
            | Self::Gauge { name, .. }
            | Self::Histogram { name, .. } => name,
        }
    }

    pub fn help(&self) -> &str {
        match self {
            Self::Counter { help, .. }
            | Self::Gauge { help, .. }
            | Self::Histogram { help, .. } => help,
        }
    }

    pub fn metric_type(&self) -> MetricType {
        match self {
            Self::Counter { .. } => MetricType::Counter,
            Self::Gauge { .. } => MetricType::Gauge,
            Self::Histogram { .. } => MetricType::Histogram,
        }
    }

    pub fn value_series(&self) -> Option<&[ValueSeries]> {
        match self {
            Self::Counter { series, .. } | Self::Gauge { series, .. } => Some(series),
            Self::Histogram { .. } => None,
        }
    }

    pub fn histogram_series(&self) -> Option<&[HistogramSeries]> {
        match self {
            Self::Histogram { series, .. } => Some(series),
            Self::Counter { .. } | Self::Gauge { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValueSeries {
    pub labels: BTreeMap<String, String>,
    pub value: u64,
}

impl ValueSeries {
    pub fn new(labels: BTreeMap<String, String>, value: u64) -> Self {
        Self { labels, value }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistogramSeries {
    pub labels: BTreeMap<String, String>,
    pub buckets: Vec<(f64, u64)>,
    pub sum: f64,
    pub count: u64,
}

impl HistogramSeries {
    pub fn new(
        labels: BTreeMap<String, String>,
        buckets: Vec<(f64, u64)>,
        sum: f64,
        count: u64,
    ) -> Self {
        Self {
            labels,
            buckets,
            sum,
            count,
        }
    }
}
