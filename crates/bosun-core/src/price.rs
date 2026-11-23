//! Prices and the bill (#3, "The bill").
//!
//! # Arithmetic
//!
//! Every published price is a whole number of micro-dollars per million tokens (the smallest,
//! Haiku 5.5's cache read, is $0.01 = 10 000), so rates are stored as integers in that unit.
//! A token count times such a rate is an integer number of picodollars (10^-12 USD), so a
//! [`Cost`] is exact and adding costs is exact. An [`Eqv`] is a cost divided by the reference
//! model's input rate, kept as that exact fraction; it converts to a float only for display.
//! Equality tests over sums therefore never depend on rounding.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Add;

use crate::event::Tokens;

/// The model id of client-side error messages, which cost nothing (#3).
pub const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Prices of the five token categories in micro-dollars (10^-6 USD) per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Rates {
    pub input: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Rates {
    /// The cost of `tokens` at these rates.
    #[must_use]
    pub fn cost(&self, tokens: &Tokens) -> Cost {
        let cache_write_1h = if cfg!(mutant_core_1h_as_5m) {
            self.cache_write_5m
        } else {
            self.cache_write_1h
        };
        let pairs = [
            (tokens.input, self.input),
            (tokens.cache_write_5m, self.cache_write_5m),
            (tokens.cache_write_1h, cache_write_1h),
            (tokens.cache_read, self.cache_read),
            (tokens.output, self.output),
        ];
        Cost {
            picodollars: pairs
                .iter()
                .map(|&(n, rate)| u128::from(n) * u128::from(rate))
                .sum(),
        }
    }
}

/// One row of the price table: a model's rates over a validity interval, optionally for
/// prompts above a size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceRow {
    /// The model id as transcripts name it.
    pub model: String,
    /// Start of validity, inclusive, milliseconds since the Unix epoch, UTC.
    pub valid_from_ms: i64,
    /// End of validity, exclusive; `None` while the row is current.
    pub valid_to_ms: Option<i64>,
    /// When set, the row applies only to a message whose prompt size is strictly greater.
    pub prompt_threshold: Option<u64>,
    pub rates: Rates,
}

impl PriceRow {
    fn valid_at(&self, ts_ms: i64) -> bool {
        self.valid_from_ms <= ts_ms && self.valid_to_ms.is_none_or(|to| ts_ms < to)
    }

    fn overlaps(&self, other: &Self) -> bool {
        let ends_before =
            |a: &Self, b: &Self| a.valid_to_ms.is_some_and(|to| to <= b.valid_from_ms);
        !ends_before(self, other) && !ends_before(other, self)
    }
}

/// An exact amount of money, in picodollars (10^-12 USD).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cost {
    pub picodollars: u128,
}

impl Cost {
    pub const ZERO: Self = Self { picodollars: 0 };

    /// The amount in dollars, rounded to a float for display.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Display only; sums use the exact integer.
    pub fn usd(&self) -> f64 {
        self.picodollars as f64 / 1e12
    }
}

impl Add for Cost {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            picodollars: self.picodollars + other.picodollars,
        }
    }
}

impl std::iter::Sum for Cost {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

/// Input-equivalent tokens: a cost divided by the reference model's input rate, kept exact.
///
/// Two values compare by the fraction they denote, so values against different reference
/// rates compare correctly too.
#[derive(Debug, Clone, Copy)]
pub struct Eqv {
    cost: Cost,
    /// The reference model's input rate, micro-dollars per million tokens; never zero.
    reference_input: u64,
}

impl Eqv {
    /// The cost this value was computed from.
    #[must_use]
    pub fn cost(&self) -> Cost {
        self.cost
    }

    /// The reference input rate it is measured against.
    #[must_use]
    pub fn reference_input(&self) -> u64 {
        self.reference_input
    }

    /// The number of input-equivalent tokens, rounded to a float for display.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Display only; comparisons use the exact fraction.
    pub fn tokens(&self) -> f64 {
        self.cost.picodollars as f64 / self.reference_input as f64
    }

    /// `self.cost * other.reference_input`, the cross product used to compare fractions.
    fn cross(&self, other: &Self) -> u128 {
        self.cost.picodollars * u128::from(other.reference_input)
    }
}

impl PartialEq for Eqv {
    fn eq(&self, other: &Self) -> bool {
        self.cross(other) == other.cross(self)
    }
}

impl Eq for Eqv {}

impl PartialOrd for Eqv {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Eqv {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cross(other).cmp(&other.cross(self))
    }
}

impl Add for Eqv {
    type Output = Self;
    /// Exact for values against one reference rate (the usual case: one table). Against
    /// different rates the sum is re-expressed against `self`'s rate, which rounds down.
    fn add(self, other: Self) -> Self {
        let other_cost = if other.reference_input == self.reference_input {
            other.cost.picodollars
        } else {
            other.cross(&self) / u128::from(other.reference_input)
        };
        Self {
            cost: Cost {
                picodollars: self.cost.picodollars + other_cost,
            },
            reference_input: self.reference_input,
        }
    }
}

/// How a message is priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pricing<'a> {
    /// The model `<synthetic>`: free.
    Synthetic,
    /// The row that applies.
    Row(&'a PriceRow),
    /// No row of the table prices this model at this time.
    Unknown,
}

/// No price for a model at a time. Reported, never priced as zero silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownModel {
    pub model: String,
    pub ts_ms: i64,
}

impl fmt::Display for UnknownModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no price for model {} at {} ms", self.model, self.ts_ms)
    }
}

impl std::error::Error for UnknownModel {}

/// A price table that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// The reference model has no row without a threshold, or its input rate is zero.
    NoReferencePrice(String),
    /// A row ends before it starts.
    EmptyInterval(String),
    /// Two rows of one model and threshold are valid at the same time.
    Overlap(String),
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoReferencePrice(m) => write!(f, "reference model {m} has no input price"),
            Self::EmptyInterval(m) => write!(f, "a row of {m} ends before it starts"),
            Self::Overlap(m) => write!(f, "rows of {m} overlap in time"),
        }
    }
}

impl std::error::Error for TableError {}

/// The versioned price table and its reference model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceTable {
    rows: Vec<PriceRow>,
    reference_model: String,
    reference_input: u64,
}

impl PriceTable {
    /// Builds a table. The eqv denominator is the input rate of the reference model's most
    /// recent row without a threshold: one fixed rate per table, so eqv adds across models and
    /// stays comparable over time. Changing the reference model scales every eqv by one factor.
    ///
    /// # Errors
    ///
    /// [`TableError`] when a row's interval is empty, two rows of one model and threshold
    /// overlap, or the reference model has no usable input rate.
    pub fn new(rows: Vec<PriceRow>, reference_model: &str) -> Result<Self, TableError> {
        for (i, row) in rows.iter().enumerate() {
            if row.valid_to_ms.is_some_and(|to| to <= row.valid_from_ms) {
                return Err(TableError::EmptyInterval(row.model.clone()));
            }
            let clash = rows[..i].iter().any(|earlier| {
                earlier.model == row.model
                    && earlier.prompt_threshold == row.prompt_threshold
                    && earlier.overlaps(row)
            });
            if clash {
                return Err(TableError::Overlap(row.model.clone()));
            }
        }
        let reference_input = rows
            .iter()
            .filter(|r| r.model == reference_model && r.prompt_threshold.is_none())
            .max_by_key(|r| r.valid_from_ms)
            .map(|r| r.rates.input)
            .filter(|&input| input > 0)
            .ok_or_else(|| TableError::NoReferencePrice(reference_model.to_owned()))?;
        Ok(Self {
            rows,
            reference_model: reference_model.to_owned(),
            reference_input,
        })
    }

    /// The model every eqv is measured against.
    #[must_use]
    pub fn reference_model(&self) -> &str {
        &self.reference_model
    }

    #[must_use]
    pub fn rows(&self) -> &[PriceRow] {
        &self.rows
    }

    /// The row that prices a message of `model` at `ts_ms` whose prompt size is
    /// `prompt_size`: among the rows valid at that time, the one with the highest threshold
    /// the prompt exceeds, else the one without a threshold.
    ///
    /// A model id with a date suffix (`claude-haiku-4-5-20251001`) that has no row of its own
    /// is priced as the id without it.
    #[must_use]
    pub fn select(&self, model: &str, ts_ms: i64, prompt_size: u64) -> Pricing<'_> {
        if model == SYNTHETIC_MODEL {
            return Pricing::Synthetic;
        }
        let row = self
            .select_exact(model, ts_ms, prompt_size)
            .or_else(|| self.select_exact(strip_date(model)?, ts_ms, prompt_size));
        row.map_or(Pricing::Unknown, Pricing::Row)
    }

    fn select_exact(&self, model: &str, ts_ms: i64, prompt_size: u64) -> Option<&PriceRow> {
        let applies = |row: &&PriceRow| match row.prompt_threshold {
            None => true,
            Some(_) if cfg!(mutant_core_no_threshold) => false,
            Some(threshold) => prompt_size > threshold,
        };
        self.rows
            .iter()
            .filter(|row| row.model == model && row.valid_at(ts_ms))
            .filter(applies)
            .max_by_key(|row| row.prompt_threshold)
    }

    /// The cost of one message.
    ///
    /// # Errors
    ///
    /// [`UnknownModel`] when no row prices `model` at `ts_ms`.
    pub fn cost(&self, model: &str, ts_ms: i64, tokens: &Tokens) -> Result<Cost, UnknownModel> {
        match self.select(model, ts_ms, tokens.prompt_size()) {
            Pricing::Synthetic => Ok(Cost::ZERO),
            Pricing::Row(row) => Ok(row.rates.cost(tokens)),
            Pricing::Unknown => Err(UnknownModel {
                model: model.to_owned(),
                ts_ms,
            }),
        }
    }

    /// `cost` in input-equivalent tokens of the reference model.
    #[must_use]
    pub fn eqv(&self, cost: Cost) -> Eqv {
        Eqv {
            cost,
            reference_input: self.reference_input,
        }
    }
}

/// `name-YYYYMMDD` to `name`; `None` when there is no such suffix.
fn strip_date(model: &str) -> Option<&str> {
    let (name, date) = model.rsplit_once('-')?;
    (date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

/// The model every eqv of the default table is measured against. Changing it scales every
/// eqv by one factor (the ratio of the two models' input rates) and changes nothing else.
pub const DEFAULT_REFERENCE_MODEL: &str = "claude-opus-5-5";

/// Rates from the input, cache-read and output prices, with the cache writes at the published
/// multiples of input (5 min 1.25x, 1 h 2x). All in micro-dollars per million tokens.
const fn rates(input: u64, cache_read: u64, output: u64) -> Rates {
    Rates {
        input,
        cache_write_5m: input * 5 / 4,
        cache_write_1h: input * 2,
        cache_read,
        output,
    }
}

/// The default price table.
///
/// Source: <https://platform.claude.com/docs/en/about-claude/pricing>, "Model pricing",
/// retrieved 2026-11-23. Standard (global, non-batch, non-fast) API prices. Every row is valid
/// from the epoch with no end: the page gives no history, and a later price change adds a row
/// with `valid_from_ms` set and closes the old one.
///
/// Cache reads are 0.1x input except Opus 5.5 and Sonnet 5.5 (0.05x) and Fable 5.1 and 5.2
/// (0.025x). Haiku 5.5 has a second row for prompts over 100 000 tokens (5x every category).
///
/// # Panics
///
/// Never: the rows are constants that pass [`PriceTable::new`] (a test asserts it).
#[must_use]
pub fn default_table() -> PriceTable {
    // $1 per million tokens.
    const D: u64 = 1_000_000;
    let rows = [
        ("claude-fable-5-2", None, rates(8 * D, D / 5, 40 * D)),
        ("claude-fable-5-1", None, rates(10 * D, D / 4, 50 * D)),
        ("claude-fable-5", None, rates(10 * D, D, 50 * D)),
        ("claude-opus-5-5", None, rates(4 * D, D / 5, 20 * D)),
        ("claude-opus-5", None, rates(5 * D, D / 2, 25 * D)),
        ("claude-sonnet-5-5", None, rates(2 * D, D / 10, 10 * D)),
        ("claude-sonnet-5", None, rates(2 * D, D / 5, 10 * D)),
        ("claude-haiku-5-5", None, rates(D / 10, D / 100, D / 2)),
        (
            "claude-haiku-5-5",
            Some(100_000),
            rates(D / 2, D / 20, 5 * D / 2),
        ),
        ("claude-haiku-4-5", None, rates(D, D / 10, 5 * D)),
    ];
    let rows = rows
        .into_iter()
        .map(|(model, prompt_threshold, rates)| PriceRow {
            model: model.to_owned(),
            valid_from_ms: 0,
            valid_to_ms: None,
            prompt_threshold,
            rates,
        })
        .collect();
    PriceTable::new(rows, DEFAULT_REFERENCE_MODEL).expect("the default table is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTOK: u64 = 1_000_000;
    /// One dollar per million tokens, in the unit of [`Rates`].
    const DOLLAR: u64 = 1_000_000;

    fn tokens(input: u64, w5: u64, w1: u64, read: u64, output: u64) -> Tokens {
        Tokens {
            input,
            cache_write_5m: w5,
            cache_write_1h: w1,
            cache_read: read,
            output,
        }
    }

    fn dollars(cost: Cost) -> u128 {
        assert_eq!(cost.picodollars % 1_000_000_000_000, 0, "{cost:?}");
        cost.picodollars / 1_000_000_000_000
    }

    #[test]
    fn default_table_matches_the_published_prices() {
        let t = default_table();
        let rates_of = |m: &str| match t.select(m, 1_790_000_000_000, 0) {
            Pricing::Row(row) => row.rates,
            other => panic!("{m}: {other:?}"),
        };
        // Dollars per MTok as published, in cents to stay integral.
        let cents = |r: Rates| {
            [
                r.input,
                r.cache_write_5m,
                r.cache_write_1h,
                r.cache_read,
                r.output,
            ]
            .map(|x| x / (DOLLAR / 100))
        };
        assert_eq!(
            cents(rates_of("claude-opus-5-5")),
            [400, 500, 800, 20, 2000]
        );
        assert_eq!(cents(rates_of("claude-opus-5")), [500, 625, 1000, 50, 2500]);
        assert_eq!(
            cents(rates_of("claude-sonnet-5-5")),
            [200, 250, 400, 10, 1000]
        );
        assert_eq!(
            cents(rates_of("claude-sonnet-5")),
            [200, 250, 400, 20, 1000]
        );
        assert_eq!(
            cents(rates_of("claude-fable-5-2")),
            [800, 1000, 1600, 20, 4000]
        );
        assert_eq!(
            cents(rates_of("claude-fable-5-1")),
            [1000, 1250, 2000, 25, 5000]
        );
        assert_eq!(
            cents(rates_of("claude-haiku-4-5")),
            [100, 125, 200, 10, 500]
        );
        assert_eq!(
            cents(rates_of("claude-haiku-4-5-20251001")),
            [100, 125, 200, 10, 500]
        );
        let haiku = rates_of("claude-haiku-5-5");
        assert_eq!(
            [
                haiku.input,
                haiku.cache_write_5m,
                haiku.cache_write_1h,
                haiku.cache_read,
                haiku.output
            ],
            [100_000, 125_000, 200_000, 10_000, 500_000]
        );
        assert_eq!(t.reference_model(), "claude-opus-5-5");
    }

    #[test]
    fn cost_prices_each_category() {
        let t = default_table();
        // Opus 5.5: $4 input, $5 5-min write, $8 1-h write, $0.20 read, $20 output.
        let cost = |tk| dollars(t.cost("claude-opus-5-5", 0, &tk).unwrap());
        assert_eq!(cost(tokens(MTOK, 0, 0, 0, 0)), 4);
        assert_eq!(cost(tokens(0, MTOK, 0, 0, 0)), 5);
        assert_eq!(cost(tokens(0, 0, MTOK, 0, 0)), 8);
        assert_eq!(cost(tokens(0, 0, 0, 5 * MTOK, 0)), 1);
        assert_eq!(cost(tokens(0, 0, 0, 0, MTOK)), 20);
        assert_eq!(cost(tokens(MTOK, MTOK, MTOK, 5 * MTOK, MTOK)), 38);
    }

    fn row(
        model: &str,
        from: i64,
        to: Option<i64>,
        threshold: Option<u64>,
        input: u64,
    ) -> PriceRow {
        PriceRow {
            model: model.to_owned(),
            valid_from_ms: from,
            valid_to_ms: to,
            prompt_threshold: threshold,
            rates: Rates {
                input: input * DOLLAR,
                ..Rates::default()
            },
        }
    }

    fn input_rate(p: Pricing<'_>) -> u64 {
        match p {
            Pricing::Row(row) => row.rates.input / DOLLAR,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_version_valid_at_the_event_time_is_used() {
        let t = PriceTable::new(
            vec![
                row("m", 0, Some(1_000), None, 3),
                row("m", 1_000, None, None, 2),
            ],
            "m",
        )
        .unwrap();
        assert_eq!(input_rate(t.select("m", 999, 0)), 3);
        assert_eq!(input_rate(t.select("m", 1_000, 0)), 2);
        assert_eq!(t.select("m", -1, 0), Pricing::Unknown);
        let one = tokens(MTOK, 0, 0, 0, 0);
        assert_eq!(dollars(t.cost("m", 500, &one).unwrap()), 3);
        assert_eq!(dollars(t.cost("m", 5_000, &one).unwrap()), 2);
    }

    #[test]
    fn prompts_over_the_threshold_use_the_threshold_row() {
        let t = default_table();
        let at = |prompt| input_rate_micro(t.select("claude-haiku-5-5", 0, prompt));
        assert_eq!(at(100_000), 100_000);
        assert_eq!(at(100_001), 500_000);

        // The prompt size counts input, cache reads and cache writes.
        let small = tokens(1, 0, 0, 99_999, 1_000);
        let large = tokens(1, 0, 1, 99_999, 1_000);
        let small_cost = t.cost("claude-haiku-5-5", 0, &small).unwrap();
        let large_cost = t.cost("claude-haiku-5-5", 0, &large).unwrap();
        assert!(large_cost.picodollars > 4 * small_cost.picodollars);
    }

    fn input_rate_micro(p: Pricing<'_>) -> u64 {
        match p {
            Pricing::Row(row) => row.rates.input,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn synthetic_is_free_and_unknown_is_reported() {
        let t = default_table();
        let some = tokens(10, 10, 10, 10, 10);
        assert_eq!(t.cost(SYNTHETIC_MODEL, 0, &some), Ok(Cost::ZERO));
        assert_eq!(
            t.cost("claude-unknown-9", 7, &some),
            Err(UnknownModel {
                model: "claude-unknown-9".to_owned(),
                ts_ms: 7
            })
        );
        assert_eq!(t.select("claude-opus-5-5-x", 0, 0), Pricing::Unknown);
    }

    #[test]
    fn invalid_tables_are_refused() {
        assert!(matches!(
            PriceTable::new(vec![row("m", 0, None, None, 1)], "other"),
            Err(TableError::NoReferencePrice(_))
        ));
        assert!(matches!(
            PriceTable::new(vec![row("m", 5, Some(5), None, 1)], "m"),
            Err(TableError::EmptyInterval(_))
        ));
        assert!(matches!(
            PriceTable::new(
                vec![row("m", 0, Some(10), None, 1), row("m", 9, None, None, 2)],
                "m"
            ),
            Err(TableError::Overlap(_))
        ));
        // A threshold row may share the interval of the base row.
        assert!(
            PriceTable::new(
                vec![row("m", 0, None, None, 1), row("m", 0, None, Some(9), 2)],
                "m"
            )
            .is_ok()
        );
    }

    /// A set of messages over several models, as (model, tokens).
    fn mixed() -> Vec<(&'static str, Tokens)> {
        vec![
            ("claude-opus-5-5", tokens(3, 1_200, 30_000, 250_000, 900)),
            ("claude-sonnet-5", tokens(7, 0, 4_000, 90_000, 1_500)),
            ("claude-fable-5-2", tokens(1, 77, 0, 400_000, 3_100)),
            ("claude-haiku-5-5", tokens(2, 0, 10, 120_000, 60)),
            ("claude-haiku-4-5-20251001", tokens(9, 900, 0, 1_000, 33)),
            (SYNTHETIC_MODEL, tokens(0, 0, 0, 0, 0)),
        ]
    }

    #[test]
    fn eqv_of_a_mixed_set_is_the_sum_of_per_model_eqv() {
        let t = default_table();
        let costs: Vec<Cost> = mixed()
            .iter()
            .map(|(m, tk)| t.cost(m, 0, tk).unwrap())
            .collect();
        let whole = t.eqv(costs.iter().copied().sum());
        let parts = costs.iter().map(|&c| t.eqv(c)).reduce(Add::add).unwrap();
        assert_eq!(whole, parts);
        assert_eq!(whole.cost(), parts.cost());
    }

    #[test]
    fn changing_the_reference_model_scales_every_eqv_by_one_factor() {
        let opus = default_table();
        let sonnet = PriceTable::new(opus.rows().to_vec(), "claude-sonnet-5-5").unwrap();
        // Opus 5.5 input is $4, Sonnet 5.5 input $2: every eqv doubles, exactly.
        for (m, tk) in mixed() {
            let cost = opus.cost(m, 0, &tk).unwrap();
            let (a, b) = (opus.eqv(cost), sonnet.eqv(cost));
            assert_eq!(b.cost(), a.cost());
            assert_eq!(
                u128::from(a.reference_input()),
                2 * u128::from(b.reference_input())
            );
            assert_eq!(b, a + a);
        }
    }
}
