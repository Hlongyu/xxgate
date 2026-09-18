use crate::{Error, Result, types::ModelRef, usage::Usage};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rates {
    pub input_per_million: Decimal,
    pub cached_input_per_million: Decimal,
    pub output_per_million: Decimal,
    pub image_input_per_million: Option<Decimal>,
    pub image_output_per_million: Option<Decimal>,
    pub per_image: Option<Decimal>,
}

impl Rates {
    pub fn multiplied(&self, factor: Decimal) -> Self {
        Self {
            input_per_million: self.input_per_million * factor,
            cached_input_per_million: self.cached_input_per_million * factor,
            output_per_million: self.output_per_million * factor,
            image_input_per_million: self.image_input_per_million.map(|x| x * factor),
            image_output_per_million: self.image_output_per_million.map(|x| x * factor),
            per_image: self.per_image.map(|x| x * factor),
        }
    }
    pub fn validate(&self) -> Result<()> {
        let mut values = vec![
            self.input_per_million,
            self.cached_input_per_million,
            self.output_per_million,
        ];
        values.extend(
            [
                self.image_input_per_million,
                self.image_output_per_million,
                self.per_image,
            ]
            .into_iter()
            .flatten(),
        );
        if values
            .iter()
            .any(|v| *v < Decimal::ZERO || *v > Decimal::from(1_000_000))
        {
            return Err(Error::invalid("Prices must be between 0 and 1000000 CNY"));
        }
        if self.per_image.is_some()
            && (self.image_input_per_million.is_some() || self.image_output_per_million.is_some())
        {
            return Err(Error::invalid(
                "Use image token rates or a per-image rate, not both",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub version: i64,
    pub model: ModelRef,
    pub standard: Rates,
    #[serde(default)]
    pub fast_multiplier: Option<Decimal>,
}
impl Price {
    pub fn validate(&self) -> Result<()> {
        self.standard.validate()?;
        if !self
            .fast_multiplier
            .is_some_and(|v| v > Decimal::ZERO && v <= Decimal::from(100))
        {
            return Err(Error::invalid("Fast 倍率必须大于 0 且不超过 100。"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Valuation {
    pub status: String,
    pub price_version: Option<i64>,
    pub cny: Option<Decimal>,
    pub items: Vec<PriceLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLine {
    pub kind: String,
    pub quantity: u64,
    pub rate: Decimal,
    pub cny: Decimal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPrice {
    pub version: i64,
    pub per_call: Option<Decimal>,
}

impl SearchPrice {
    pub fn validate(&self) -> Result<()> {
        if self
            .per_call
            .is_some_and(|v| v < Decimal::ZERO || v > Decimal::from(1_000_000) || v.scale() > 8)
        {
            return Err(Error::invalid(
                "Search 单价应为 0 至 1000000 元，最多 8 位小数",
            ));
        }
        Ok(())
    }
}

pub fn value_search(calls: u64, price: Option<&SearchPrice>) -> Valuation {
    let mut value = Valuation {
        status: "not_charged".into(),
        price_version: None,
        cny: Some(Decimal::ZERO),
        items: vec![],
    };
    if calls == 0 {
        return value;
    }
    let Some(rate) = price.and_then(|p| p.per_call) else {
        value.status = "unpriced_search".into();
        value.cny = None;
        return value;
    };
    let cny = (Decimal::from(calls) * rate).round_dp(8);
    value.status = "priced".into();
    value.cny = Some(cny);
    value.items.push(PriceLine {
        kind: "search".into(),
        quantity: calls,
        rate,
        cny,
    });
    value
}

pub fn value_usage(
    usage: &Usage,
    requested_tier: Option<&str>,
    price: Option<&Price>,
) -> Valuation {
    let mut result = Valuation {
        status: "usage_unknown".into(),
        price_version: price.map(|p| p.version),
        cny: None,
        items: vec![],
    };
    let Some(input) = usage.input_tokens else {
        return result;
    };
    let Some(output) = usage.output_tokens else {
        return result;
    };
    let Some(price) = price else {
        result.status = "unpriced".into();
        return result;
    };
    let actual = usage.service_tier.as_deref();
    if actual.is_none() && matches!(requested_tier, Some("fast" | "priority")) {
        result.status = "tier_unknown".into();
        return result;
    }
    let fast_rates;
    let rates = if matches!(actual, Some("fast" | "priority")) {
        let Some(factor) = price
            .fast_multiplier
            .filter(|v| *v > Decimal::ZERO && *v <= Decimal::from(100))
        else {
            result.status = "unpriced_fast".into();
            return result;
        };
        fast_rates = price.standard.multiplied(factor);
        &fast_rates
    } else {
        &price.standard
    };
    let cached = match usage.cached_input_tokens {
        Some(cached) => cached,
        None if input == 0 || rates.input_per_million == rates.cached_input_per_million => 0,
        None => {
            result.status = "cache_usage_unknown".into();
            return result;
        }
    };
    if cached > input || usage.reasoning_output_tokens.is_some_and(|n| n > output) {
        result.status = "invalid_usage".into();
        return result;
    }
    let mut add = |kind: &str, count: u64, rate: Decimal, per_million: bool| {
        let cost = Decimal::from(count) * rate
            / if per_million {
                Decimal::from(1_000_000)
            } else {
                Decimal::ONE
            };
        result.items.push(PriceLine {
            kind: kind.into(),
            quantity: count,
            rate,
            cny: cost.round_dp(8),
        });
    };
    add("input", input - cached, rates.input_per_million, true);
    add("cached_input", cached, rates.cached_input_per_million, true);
    add("output", output, rates.output_per_million, true);
    let mut image_complete = true;
    if usage.image_count > 0 {
        if let Some(rate) = rates.per_image {
            add("image", u64::from(usage.image_count), rate, false);
        } else if usage.image_tool_usage_reported {
            for (kind, n, rate) in [
                (
                    "image_input",
                    usage.image_input_tokens,
                    rates.image_input_per_million,
                ),
                (
                    "image_output",
                    usage.image_output_tokens,
                    rates.image_output_per_million,
                ),
            ] {
                match (n, rate) {
                    (Some(n), Some(rate)) => add(kind, n, rate, true),
                    (Some(0), _) => {}
                    _ => image_complete = false,
                }
            }
        } else {
            image_complete = false;
        }
    }
    result.status = if !image_complete {
        "image_unpriced"
    } else if usage.complete {
        "priced"
    } else {
        "partial"
    }
    .into();
    if image_complete {
        result.cny = Some(result.items.iter().map(|x| x.cny).sum());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_pricing_distinguishes_unconfigured_free_and_failed_calls() {
        assert_eq!(value_search(1, None).status, "unpriced_search");
        assert_eq!(value_search(0, None).cny, Some(Decimal::ZERO));
        let mut price = SearchPrice {
            version: 7,
            per_call: Some(Decimal::new(12345678, 8)),
        };
        let saved = value_search(2, Some(&price));
        assert_eq!(saved.cny, Some(Decimal::new(24691356, 8)));
        assert_eq!(saved.items[0].quantity, 2);
        price.per_call = Some(Decimal::ZERO);
        assert_eq!(value_search(1, Some(&price)).status, "priced");
        assert_eq!(saved.items[0].rate, Decimal::new(12345678, 8));
        for invalid in [
            Decimal::NEGATIVE_ONE,
            Decimal::from(1_000_001),
            Decimal::new(1, 9),
        ] {
            price.per_call = Some(invalid);
            assert!(price.validate().is_err());
        }
    }
    fn price() -> Price {
        Price {
            version: 1,
            model: ModelRef::codex("test"),
            standard: Rates {
                input_per_million: Decimal::from(10),
                cached_input_per_million: Decimal::ONE,
                output_per_million: Decimal::from(20),
                image_input_per_million: None,
                image_output_per_million: None,
                per_image: None,
            },
            fast_multiplier: Some(Decimal::from(2)),
        }
    }
    #[test]
    fn subsets_are_not_added_twice() {
        let u = Usage {
            input_tokens: Some(1000),
            cached_input_tokens: Some(500),
            output_tokens: Some(1000),
            reasoning_output_tokens: Some(800),
            complete: true,
            ..Usage::default()
        };
        assert_eq!(
            value_usage(&u, None, Some(&price())).cny.unwrap(),
            Decimal::new(255, 4)
        );
        assert_eq!(
            value_usage(&u, Some("fast"), Some(&price())).status,
            "tier_unknown"
        );
        assert!(
            value_usage(&Usage::default(), None, Some(&price()))
                .cny
                .is_none()
        );
    }
    #[test]
    fn fast_multiplies_all_standard_rates_and_is_frozen_in_valuation() {
        let mut p = price();
        p.fast_multiplier = Some(Decimal::new(25, 1));
        p.standard.per_image = Some(Decimal::new(15, 1));
        let usage = Usage {
            input_tokens: Some(1000),
            cached_input_tokens: Some(500),
            output_tokens: Some(1000),
            image_count: 2,
            service_tier: Some("priority".into()),
            complete: true,
            ..Default::default()
        };
        let valuation = value_usage(&usage, Some("priority"), Some(&p));
        assert_eq!(valuation.cny.unwrap(), Decimal::new(756375, 5));
        assert_eq!(
            valuation
                .items
                .iter()
                .find(|i| i.kind == "image")
                .unwrap()
                .rate,
            Decimal::new(375, 2)
        );
        p.fast_multiplier = Some(Decimal::from(3));
        assert_eq!(valuation.cny.unwrap(), Decimal::new(756375, 5));
        assert_ne!(
            value_usage(&usage, Some("priority"), Some(&p)).cny,
            valuation.cny
        );
    }
    #[test]
    fn invalid_multiplier_is_rejected_and_legacy_price_has_no_invented_factor() {
        let mut p = price();
        for multiplier in [Decimal::ZERO, Decimal::NEGATIVE_ONE, Decimal::from(101)] {
            p.fast_multiplier = Some(multiplier);
            assert!(p.validate().is_err());
        }
        let mut legacy = serde_json::to_value(price()).unwrap();
        legacy.as_object_mut().unwrap().remove("fast_multiplier");
        legacy["fast"] = serde_json::to_value(price().standard).unwrap();
        let legacy: Price = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.fast_multiplier, None);
        let usage = Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            service_tier: Some("priority".into()),
            ..Default::default()
        };
        assert_eq!(
            value_usage(&usage, Some("priority"), Some(&legacy)).status,
            "unpriced_fast"
        );
    }
}
