//! `DogStatsD` payload.

use std::{fmt, io::Write, num::NonZeroUsize, ops::Range};

use rand::{distributions::WeightedIndex, prelude::Distribution, seq::SliceRandom, Rng};
use serde::Deserialize;

use crate::payload::{Error, Serialize};

use self::{
    common::tags, event::EventGenerator, metric::MetricGenerator,
    service_check::ServiceCheckGenerator,
};

use super::{common::AsciiString, Generator};

mod common;
mod event;
mod metric;
mod service_check;

fn contexts_minimum() -> NonZeroUsize {
    NonZeroUsize::new(5000).unwrap()
}

fn contexts_maximum() -> NonZeroUsize {
    NonZeroUsize::new(10000).unwrap()
}

fn tags_per_msg_minimum() -> NonZeroUsize {
    NonZeroUsize::new(5000).unwrap()
}

fn tags_per_msg_maximum() -> NonZeroUsize {
    NonZeroUsize::new(10000).unwrap()
}

fn multivalue_pack_probability() -> f32 {
    0.08
}

fn multivalue_cnt_minimum() -> NonZeroUsize {
    NonZeroUsize::new(2).unwrap()
}

fn multivalue_cnt_maximum() -> NonZeroUsize {
    NonZeroUsize::new(32).unwrap()
}

/// Weights for `DogStatsD` kinds: metrics, events, service checks
///
/// Defines the relative probability of each kind of `DogStatsD` datagram.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct KindWeights {
    metric: u8,
    event: u8,
    service_check: u8,
}

impl Default for KindWeights {
    fn default() -> Self {
        KindWeights {
            metric: 80,        // 80%
            event: 10,         // 10%
            service_check: 10, // 10%
        }
    }
}

/// Weights for `DogStatsD` metrics: gauges, counters, etc
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MetricWeights {
    count: u8,
    gauge: u8,
    timer: u8,
    distribution: u8,
    set: u8,
    histogram: u8,
}

impl Default for MetricWeights {
    fn default() -> Self {
        MetricWeights {
            count: 34,       // 34%
            gauge: 34,       // 34%
            timer: 5,        // 5%
            distribution: 1, // 1%
            set: 1,          // 1%
            histogram: 25,   // 25%
        }
    }
}

/// Configure the `DogStatsD` payload.
#[derive(Debug, Deserialize, Clone, PartialEq, Copy)]
pub struct Config {
    /// Minimum number of unique metric contexts to generate
    /// A context is a set of unique metric name + tags
    #[serde(default = "contexts_minimum")]
    pub contexts_minimum: NonZeroUsize,

    /// Maximum number of unique metric contexts to generate
    /// A context is a set of unique metric name + tags
    #[serde(default = "contexts_maximum")]
    pub contexts_maximum: NonZeroUsize,

    /// Maximum number of tags per individual dogstatsd msg
    /// a tag is a key-value pair separated by a :
    #[serde(default = "tags_per_msg_maximum")]
    pub tags_per_msg_maximum: NonZeroUsize,

    /// Minimum number of tags per individual dogstatsd msg
    /// a tag is a key-value pair separated by a :
    #[serde(default = "tags_per_msg_minimum")]
    pub tags_per_msg_minimum: NonZeroUsize,

    /// Probability between 0 and 1 that a given dogstatsd msg
    /// contains multiple values
    #[serde(default = "multivalue_pack_probability")]
    pub multivalue_pack_probability: f32,

    /// The minimum count of values that will be generated if
    /// multi-value is chosen to be generated
    #[serde(default = "multivalue_cnt_minimum")]
    pub multivalue_cnt_minimum: NonZeroUsize,

    /// The maximum count of values that will be generated if
    /// multi-value is chosen to be generated
    #[serde(default = "multivalue_cnt_maximum")]
    pub multivalue_cnt_maximum: NonZeroUsize,

    /// Defines the relative probability of each kind of DogStatsD kinds of
    /// payload.
    #[serde(default)]
    pub kind_weights: KindWeights,
    /// Defines the relative probability of each kind of DogStatsD metric.
    #[serde(default)]
    pub metric_weights: MetricWeights,
}

fn choose_or_not<R, T>(mut rng: &mut R, pool: &[T]) -> Option<T>
where
    T: Clone,
    R: rand::Rng + ?Sized,
{
    if rng.gen() {
        pool.choose(&mut rng).cloned()
    } else {
        None
    }
}

#[derive(Debug, Clone)]
struct MemberGenerator {
    kind_weights: WeightedIndex<u8>,
    string_pool: StringPool<'a>,
    event_generator: EventGenerator<'a>,
    service_check_generator: ServiceCheckGenerator<'a>,
    metric_generator: MetricGenerator<'a>,
}

#[inline]
/// Generate a total number of strings between min and max with a maximum length
/// per string of `max_length`.
fn random_strings_with_length<R>(min_max: Range<usize>, max_length: u16, rng: &mut R) -> Vec<String>
where
    R: Rng + ?Sized,
{
    let mut buf = Vec::with_capacity(min_max.end);
    for _ in 0..rng.gen_range(min_max) {
        buf.push(AsciiString::with_maximum_length(max_length).generate(rng));
    }
    buf
}

#[inline]
/// Generate a `total` number of strings with a maximum length per string of
/// `max_length`.
fn random_strings<R>(total: usize, max_length: u16, rng: &mut R) -> Vec<String>
where
    R: Rng + ?Sized,
{
    let mut buf = Vec::with_capacity(total);
    for _ in 0..total {
        buf.push(AsciiString::with_maximum_length(max_length).generate(rng));
    }
    buf
}

impl MemberGenerator {
    fn new<R>(
        context_range: Range<NonZeroUsize>,
        tags_per_msg_range: Range<NonZeroUsize>,
        multivalue_cnt_range: Range<NonZeroUsize>,
        multivalue_pack_probability: f32,
        kind_weights: KindWeights,
        metric_weights: MetricWeights,
        mut rng: &mut R,
    ) -> Self
    where
        R: Rng + ?Sized,
    {
        let context_range: Range<usize> =
            context_range.start.try_into().unwrap()..context_range.end.try_into().unwrap();

        let tags_per_msg_range: Range<usize> = tags_per_msg_range.start.try_into().unwrap()
            ..tags_per_msg_range.end.try_into().unwrap();

        let num_contexts = rng.gen_range(context_range);

        // TODO pick a value for this or make it configurable
        let max_tag_length = 36_u16;

        let tags_generator = tags::Generator {
            num_tagsets: num_contexts,
            tags_per_msg_range,
            max_length: max_tag_length,
        };

        let service_event_titles = random_strings(num_contexts, 64, &mut rng); // TODO assert that num_context less than 64
        let tagsets = tags_generator.generate(&mut rng);
        let texts_or_messages = random_strings_with_length(4..128, 1024, &mut rng);
        let small_strings = random_strings_with_length(16..1024, 8, &mut rng);

        // For service checks and events, there is no "aggregation" going on, so the idea of a "context"
        // does not really make sense. Therefore "titles" and "tags" can be independently chosen freely.
        let event_generator = EventGenerator {
            titles: service_event_titles.clone(),
            texts_or_messages: texts_or_messages.clone(),
            small_strings: small_strings.clone(),
            tagsets: tagsets.clone(),
        };

        let service_check_generator = ServiceCheckGenerator {
            names: service_event_titles.clone(),
            small_strings: small_strings.clone(),
            texts_or_messages,
            tagsets: tagsets.clone(),
        };

        // NOTE the ordering here of `metric_choices` is very important! If you
        // change it here you MUST also change it in `Generator<Metric> for
        // MetricGenerator`.
        let metric_choices = [
            metric_weights.count,
            metric_weights.gauge,
            metric_weights.timer,
            metric_weights.distribution,
            metric_weights.set,
            metric_weights.histogram,
        ];

        let multivalue_cnt_range: Range<usize> = multivalue_cnt_range.start.try_into().unwrap()
            ..multivalue_cnt_range.end.try_into().unwrap();

        // TODO pass in a TagsGenerator instead of the `tags_per_msg_range`
        // Its both the more correct way to do it and solves a borrow-checker problem
        let metric_generator = MetricGenerator::new(
            num_contexts,
            multivalue_cnt_range,
            multivalue_pack_probability,
            &WeightedIndex::new(metric_choices).unwrap(),
            small_strings,
            tagsets.clone(),
            &mut rng,
        );

        // NOTE the ordering here of `member_choices` is very important! If you
        // change it here you MUST also change it in `Generator<Member> for
        // MemberGenerator`.
        let member_choices = [
            kind_weights.metric,
            kind_weights.event,
            kind_weights.service_check,
        ];
        MemberGenerator {
            kind_weights: WeightedIndex::new(member_choices).unwrap(),
            event_generator,
            service_check_generator,
            metric_generator,
        }
    }
}

impl Generator<Member> for MemberGenerator {
    fn generate<R>(&self, rng: &mut R) -> Member
    where
        R: rand::Rng + ?Sized,
    {
        match self.kind_weights.sample(rng) {
            0 => Member::Metric(self.metric_generator.generate(rng)),
            1 => Member::Event(self.event_generator.generate(rng)),
            2 => Member::ServiceCheck(self.service_check_generator.generate(rng)),
            _ => unreachable!(),
        }
    }
}

// https://docs.datadoghq.com/developers/dogstatsd/datagram_shell/
#[derive(Debug)]
/// Supra-type for all dogstatsd variants
pub enum Member {
    /// Metrics
    Metric(metric::Metric),
    /// Events, think syslog.
    Event(event::Event),
    /// Services, checked.
    ServiceCheck(service_check::ServiceCheck),
}

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metric(ref m) => write!(f, "{m}"),
            Self::Event(ref e) => write!(f, "{e}"),
            Self::ServiceCheck(ref sc) => write!(f, "{sc}"),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::module_name_repetitions)]
/// A generator for `DogStatsD` payloads
pub struct DogStatsD {
    member_generator: MemberGenerator,
}

impl DogStatsD {
    /// Create a new default instance of `DogStatsD`
    pub fn default<R>(rng: &mut R) -> Self
    where
        R: rand::Rng + ?Sized,
    {
        Self::new(
            contexts_minimum()..contexts_maximum(),
            tags_per_msg_minimum()..tags_per_msg_maximum(),
            multivalue_cnt_minimum()..multivalue_cnt_maximum(),
            multivalue_pack_probability(),
            KindWeights::default(),
            MetricWeights::default(),
            rng,
        )
    }

    #[cfg(feature = "dogstatsd_perf")]
    /// Call the internal member generator and count the in-memory byte
    /// size. This is not useful except in a loop to track how quickly we can do
    /// this operation. It's meant to be a proxy by which we can determine how
    /// quickly members are able to be generated and then serialized. An
    /// approximation.
    pub fn generate<R>(&self, rng: &mut R) -> Member
    where
        R: rand::Rng + ?Sized,
    {
        self.member_generator.generate(rng)
    }

    pub(crate) fn new<R>(
        context_range: Range<NonZeroUsize>,
        tags_per_msg_range: Range<NonZeroUsize>,
        multivalue_cnt_range: Range<NonZeroUsize>,
        multivalue_pack_probability: f32,
        kind_weights: KindWeights,
        metric_weights: MetricWeights,
        rng: &mut R,
    ) -> Self
    where
        R: rand::Rng + ?Sized,
    {
        let member_generator = MemberGenerator::new(
            context_range,
            tags_per_msg_range,
            multivalue_cnt_range,
            multivalue_pack_probability,
            kind_weights,
            metric_weights,
            rng,
        );

        Self { member_generator }
    }
}

impl Serialize for DogStatsD {
    fn to_bytes<W, R>(&self, mut rng: R, max_bytes: usize, writer: &mut W) -> Result<(), Error>
    where
        R: Rng + Sized,
        W: Write,
    {
        let mut bytes_remaining = max_bytes;
        loop {
            let member: Member = self.member_generator.generate(&mut rng);
            let encoding = format!("{member}");
            let line_length = encoding.len() + 1; // add one for the newline
            match bytes_remaining.checked_sub(line_length) {
                Some(remainder) => {
                    writeln!(writer, "{encoding}")?;
                    bytes_remaining = remainder;
                }
                None => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::{rngs::SmallRng, SeedableRng};

    use crate::payload::{
        dogstatsd::{
            contexts_maximum, contexts_minimum, multivalue_cnt_maximum, multivalue_cnt_minimum,
            multivalue_pack_probability, tags_per_msg_maximum, tags_per_msg_minimum, KindWeights,
            MetricWeights,
        },
        DogStatsD, Serialize,
    };

    // We want to be sure that the serialized size of the payload does not
    // exceed `max_bytes`.
    proptest! {
        #[test]
        fn payload_not_exceed_max_bytes(seed: u64, max_bytes: u16) {
            let max_bytes = max_bytes as usize;
            let mut rng = SmallRng::seed_from_u64(seed);
            let context_range = contexts_minimum()..contexts_maximum();
            let tags_per_msg_range = tags_per_msg_minimum()..tags_per_msg_maximum();
            let multivalue_cnt_range = multivalue_cnt_minimum()..multivalue_cnt_maximum();
            let multivalue_pack_probability = multivalue_pack_probability();

            let kind_weights = KindWeights::default();
            let metric_weights = MetricWeights::default();
            let dogstatsd = DogStatsD::new(context_range, tags_per_msg_range, multivalue_cnt_range, multivalue_pack_probability, kind_weights,
                                           metric_weights, &mut rng);

            let mut bytes = Vec::with_capacity(max_bytes);
            dogstatsd.to_bytes(rng, max_bytes, &mut bytes).unwrap();
            debug_assert!(
                bytes.len() <= max_bytes,
                "{:?}",
                std::str::from_utf8(&bytes).unwrap()
            );
        }
    }
}
