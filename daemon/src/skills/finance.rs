//! Category: FINANCE — money math that is PURE ARITHMETIC over the user's own
//! inputs: tip + bill split, percentage change, simple/compound interest, loan
//! payment + amortization summary, ROI, break-even units, savings goal, rule of
//! 72, mortgage affordability. NEVER a live quote — a skill that would need a
//! live FX rate or stock price is `source_gated` (returns "needs a data source"
//! until configured) or omitted. No skill here fabricates a market value.
//!
//! Every skill is a TOTAL function of its numeric args: bounded inputs, a
//! friendly error on bad/missing args (never a panic, never a fabricated
//! number), and exact `f64` arithmetic that is reproducible call-to-call.
//! Currency-ish outputs are rounded to cents only at the boundary, so the math
//! stays honest and the tests can pin exact strings.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The finance catalog. The Library phase appends `SkillDef::new(...)` entries to
/// THIS vec (and nothing in mod.rs changes).
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "tip_split",
            Category::Finance,
            "Compute a tip and split a bill across people. Use when the user wants to tip a percentage and divide the total per person.",
            &["tip", "split the bill", "how much to tip", "divide the check", "per person"],
            tip_split,
        ),
        SkillDef::new(
            "percentage_change",
            Category::Finance,
            "Percent change from an old value to a new value. Use for 'what percent did X go up/down' between two numbers.",
            &["percent change", "percentage increase", "how much did it go up", "percent difference", "growth rate"],
            percentage_change,
        ),
        SkillDef::new(
            "simple_interest",
            Category::Finance,
            "Simple interest: interest = principal * rate * years (no compounding). Use for a flat-rate loan or deposit.",
            &["simple interest", "flat interest", "interest on a loan", "no compounding"],
            simple_interest,
        ),
        SkillDef::new(
            "compound_interest",
            Category::Finance,
            "Future value with periodic compounding. Use for a savings/investment that compounds n times per year for some years.",
            &["compound interest", "future value", "compounding", "savings growth", "investment value"],
            compound_interest,
        ),
        SkillDef::new(
            "loan_payment",
            Category::Finance,
            "Fixed monthly payment for an amortizing loan plus total paid and total interest. Use for a car/personal/mortgage payment from principal, annual rate, and months.",
            &["monthly payment", "loan payment", "amortize a loan", "what's the payment", "total interest on a loan"],
            loan_payment,
        ),
        SkillDef::new(
            "roi",
            Category::Finance,
            "Return on investment: profit and ROI% from a cost and a final value. Use for 'what's my return' on an investment.",
            &["roi", "return on investment", "profit percent", "what's my return", "gain percent"],
            roi,
        ),
        SkillDef::new(
            "break_even_units",
            Category::Finance,
            "Break-even unit count from fixed costs, price per unit, and variable cost per unit. Use to find how many units cover costs.",
            &["break even", "break-even units", "how many to sell", "cover my costs", "break even point"],
            break_even_units,
        ),
        SkillDef::new(
            "savings_goal_monthly",
            Category::Finance,
            "Monthly deposit needed to reach a savings goal in N months, given an annual rate (an annuity solve). Use for 'how much to save each month'.",
            &["savings goal", "how much to save each month", "monthly deposit to reach", "save for a goal"],
            savings_goal_monthly,
        ),
        SkillDef::new(
            "rule_of_72",
            Category::Finance,
            "Approximate years to double money at a given annual rate (the rule of 72). Use for a quick doubling-time estimate.",
            &["rule of 72", "time to double", "doubling time", "how long to double my money"],
            rule_of_72,
        ),
        SkillDef::new(
            "mortgage_affordability",
            Category::Finance,
            "Max affordable mortgage principal from a monthly housing budget, annual rate, and term (the loan-payment formula solved for principal). Use for 'how big a mortgage can I afford'.",
            &["how much house can I afford", "mortgage affordability", "max mortgage", "affordable loan amount"],
            mortgage_affordability,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Shared numeric helpers — bounded extraction so no skill panics or fabricates.
// ---------------------------------------------------------------------------

/// Pull a required finite `f64` arg by name, with a friendly per-skill error.
/// Accepts any JSON number (int or float); rejects missing / non-numeric / NaN /
/// infinite so a downstream formula can never produce a bogus value.
fn num(args: &Value, key: &str, skill: &str) -> Result<f64> {
    let v = args
        .get(key)
        .ok_or_else(|| anyhow!("{skill} needs a numeric '{key}' argument"))?;
    let n = v
        .as_f64()
        .ok_or_else(|| anyhow!("{skill} '{key}' must be a number"))?;
    if !n.is_finite() {
        return Err(anyhow!("{skill} '{key}' must be a finite number"));
    }
    Ok(n)
}

/// Pull a required positive integer count (e.g. people, units, months).
fn pos_int(args: &Value, key: &str, skill: &str, max: u64) -> Result<u64> {
    let n = args
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("{skill} needs a positive integer '{key}'"))?;
    if n == 0 {
        return Err(anyhow!("{skill} '{key}' must be at least 1"));
    }
    if n > max {
        return Err(anyhow!("{skill} '{key}' must be at most {max}"));
    }
    Ok(n)
}

/// Round to whole cents (2 decimals) at the output boundary only. `(x*100)`
/// rounded then `/100` — half-away-from-zero, matching how money is quoted.
fn cents(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Format a value as a 2-decimal money string (no symbol — the model adds the
/// currency word). Negative zero is normalized to `0.00`.
fn money(x: f64) -> String {
    let r = cents(x) + 0.0; // normalize -0.0 -> 0.0
    format!("{:.2}", if r == 0.0 { 0.0 } else { r })
}

/// Format a percentage to 2 decimals.
fn pct(x: f64) -> String {
    format!("{:.2}%", x)
}

// ---------------------------------------------------------------------------
// Skills.
// ---------------------------------------------------------------------------

/// `tip_split {bill, tip_percent, people?}` -> tip, grand total, and per-person
/// share. `people` defaults to 1. Bill must be >= 0, tip_percent in 0..=100.
fn tip_split(args: &Value) -> Result<String> {
    let bill = num(args, "bill", "tip_split")?;
    let tip_percent = num(args, "tip_percent", "tip_split")?;
    let people = args.get("people").and_then(Value::as_u64).unwrap_or(1);
    if bill < 0.0 {
        return Err(anyhow!("tip_split 'bill' must be 0 or more"));
    }
    if !(0.0..=100.0).contains(&tip_percent) {
        return Err(anyhow!("tip_split 'tip_percent' must be between 0 and 100"));
    }
    if people == 0 || people > 1000 {
        return Err(anyhow!("tip_split 'people' must be 1..=1000"));
    }
    let tip = bill * tip_percent / 100.0;
    let total = bill + tip;
    let per = total / people as f64;
    Ok(format!(
        "Bill {} + {} tip = {} total; split {} way(s) = {} each",
        money(bill),
        money(tip),
        money(total),
        people,
        money(per)
    ))
}

/// `percentage_change {old, new}` -> percent change from old to new. `old` must
/// be non-zero (percent change off zero is undefined).
fn percentage_change(args: &Value) -> Result<String> {
    let old = num(args, "old", "percentage_change")?;
    let new = num(args, "new", "percentage_change")?;
    if old == 0.0 {
        return Err(anyhow!(
            "percentage_change 'old' must be non-zero (percent change off zero is undefined)"
        ));
    }
    let change = (new - old) / old.abs() * 100.0;
    let dir = if change > 0.0 {
        "increase"
    } else if change < 0.0 {
        "decrease"
    } else {
        "no change"
    };
    Ok(format!(
        "{} -> {} is a {} {}",
        money(old),
        money(new),
        pct(change),
        dir
    ))
}

/// `simple_interest {principal, annual_rate_percent, years}` -> interest earned
/// and the final amount. No compounding: I = P * r * t.
fn simple_interest(args: &Value) -> Result<String> {
    let principal = num(args, "principal", "simple_interest")?;
    let rate = num(args, "annual_rate_percent", "simple_interest")?;
    let years = num(args, "years", "simple_interest")?;
    if principal < 0.0 {
        return Err(anyhow!("simple_interest 'principal' must be 0 or more"));
    }
    if years < 0.0 {
        return Err(anyhow!("simple_interest 'years' must be 0 or more"));
    }
    let interest = principal * (rate / 100.0) * years;
    let total = principal + interest;
    Ok(format!(
        "Simple interest on {} at {} for {} year(s): {} interest, {} total",
        money(principal),
        pct(rate),
        trim_years(years),
        money(interest),
        money(total)
    ))
}

/// `compound_interest {principal, annual_rate_percent, years, times_per_year?}`
/// -> future value and interest earned. FV = P(1 + r/n)^(n*t). `times_per_year`
/// defaults to 12 (monthly).
fn compound_interest(args: &Value) -> Result<String> {
    let principal = num(args, "principal", "compound_interest")?;
    let rate = num(args, "annual_rate_percent", "compound_interest")?;
    let years = num(args, "years", "compound_interest")?;
    let n = args
        .get("times_per_year")
        .and_then(Value::as_u64)
        .unwrap_or(12);
    if principal < 0.0 {
        return Err(anyhow!("compound_interest 'principal' must be 0 or more"));
    }
    if years < 0.0 {
        return Err(anyhow!("compound_interest 'years' must be 0 or more"));
    }
    if !(1..=365).contains(&n) {
        return Err(anyhow!("compound_interest 'times_per_year' must be 1..=365"));
    }
    let r = rate / 100.0;
    let nf = n as f64;
    let fv = principal * (1.0 + r / nf).powf(nf * years);
    let interest = fv - principal;
    Ok(format!(
        "Compounding {} at {} for {} year(s), {}x/year: {} future value ({} interest)",
        money(principal),
        pct(rate),
        trim_years(years),
        n,
        money(fv),
        money(interest)
    ))
}

/// `loan_payment {principal, annual_rate_percent, months}` -> the fixed monthly
/// payment, total paid, and total interest for an amortizing loan. Handles a 0%
/// loan as a straight-line split (avoids divide-by-zero).
fn loan_payment(args: &Value) -> Result<String> {
    let principal = num(args, "principal", "loan_payment")?;
    let rate = num(args, "annual_rate_percent", "loan_payment")?;
    let months = pos_int(args, "months", "loan_payment", 600)?;
    if principal <= 0.0 {
        return Err(anyhow!("loan_payment 'principal' must be greater than 0"));
    }
    if rate < 0.0 {
        return Err(anyhow!("loan_payment 'annual_rate_percent' must be 0 or more"));
    }
    let payment = amortized_payment(principal, rate, months);
    let total = payment * months as f64;
    let interest = total - principal;
    Ok(format!(
        "{} over {} months at {}: {}/month, {} total paid ({} interest)",
        money(principal),
        months,
        pct(rate),
        money(payment),
        money(total),
        money(interest)
    ))
}

/// The standard amortized monthly payment. With a 0% rate it is a straight-line
/// split; otherwise P * i / (1 - (1+i)^-m) where i is the monthly rate.
fn amortized_payment(principal: f64, annual_rate_percent: f64, months: u64) -> f64 {
    let m = months as f64;
    let i = annual_rate_percent / 100.0 / 12.0;
    if i == 0.0 {
        principal / m
    } else {
        principal * i / (1.0 - (1.0 + i).powf(-m))
    }
}

/// `roi {cost, final_value}` -> profit and ROI%. ROI = (final - cost)/cost*100.
/// `cost` must be positive (ROI off a zero/negative cost is undefined).
fn roi(args: &Value) -> Result<String> {
    let cost = num(args, "cost", "roi")?;
    let final_value = num(args, "final_value", "roi")?;
    if cost <= 0.0 {
        return Err(anyhow!("roi 'cost' must be greater than 0"));
    }
    let profit = final_value - cost;
    let roi_pct = profit / cost * 100.0;
    Ok(format!(
        "Cost {} -> value {}: {} profit, {} ROI",
        money(cost),
        money(final_value),
        money(profit),
        pct(roi_pct)
    ))
}

/// `break_even_units {fixed_costs, price_per_unit, variable_cost_per_unit}` ->
/// units to break even (rounded UP, since a fractional unit doesn't cover costs)
/// and the contribution margin per unit. Price must exceed variable cost.
fn break_even_units(args: &Value) -> Result<String> {
    let fixed = num(args, "fixed_costs", "break_even_units")?;
    let price = num(args, "price_per_unit", "break_even_units")?;
    let var = num(args, "variable_cost_per_unit", "break_even_units")?;
    if fixed < 0.0 {
        return Err(anyhow!("break_even_units 'fixed_costs' must be 0 or more"));
    }
    let margin = price - var;
    if margin <= 0.0 {
        return Err(anyhow!(
            "break_even_units needs 'price_per_unit' greater than 'variable_cost_per_unit' (otherwise you never break even)"
        ));
    }
    let exact = fixed / margin;
    let units = exact.ceil() as u64;
    Ok(format!(
        "Contribution margin {}/unit; break even at {} units (exact {:.2})",
        money(margin),
        units,
        exact
    ))
}

/// `savings_goal_monthly {goal, months, annual_rate_percent?}` -> the level
/// monthly deposit (made at period end) needed to reach `goal`. With a rate this
/// is the future-value-of-annuity solve; at 0% it is goal/months. Rate defaults
/// to 0.
fn savings_goal_monthly(args: &Value) -> Result<String> {
    let goal = num(args, "goal", "savings_goal_monthly")?;
    let months = pos_int(args, "months", "savings_goal_monthly", 1200)?;
    let rate = args
        .get("annual_rate_percent")
        .map(|_| num(args, "annual_rate_percent", "savings_goal_monthly"))
        .transpose()?
        .unwrap_or(0.0);
    if goal <= 0.0 {
        return Err(anyhow!("savings_goal_monthly 'goal' must be greater than 0"));
    }
    if rate < 0.0 {
        return Err(anyhow!(
            "savings_goal_monthly 'annual_rate_percent' must be 0 or more"
        ));
    }
    let m = months as f64;
    let i = rate / 100.0 / 12.0;
    // FV of an ordinary annuity = PMT * ((1+i)^m - 1)/i ; solve for PMT.
    let deposit = if i == 0.0 {
        goal / m
    } else {
        goal * i / ((1.0 + i).powf(m) - 1.0)
    };
    Ok(format!(
        "To reach {} in {} months at {}: save {}/month",
        money(goal),
        months,
        pct(rate),
        money(deposit)
    ))
}

/// `rule_of_72 {annual_rate_percent}` -> approximate years to double. 72 / rate.
/// Rate must be > 0 (you can't double at a non-positive rate).
fn rule_of_72(args: &Value) -> Result<String> {
    let rate = num(args, "annual_rate_percent", "rule_of_72")?;
    if rate <= 0.0 {
        return Err(anyhow!(
            "rule_of_72 'annual_rate_percent' must be greater than 0"
        ));
    }
    let years = 72.0 / rate;
    Ok(format!(
        "At {}/year, money roughly doubles in {:.1} years (rule of 72)",
        pct(rate),
        years
    ))
}

/// `mortgage_affordability {monthly_budget, annual_rate_percent, years}` -> the
/// largest principal whose amortized payment fits the monthly budget. This is the
/// loan-payment formula solved for P: at 0% it's budget*months; otherwise
/// PMT*(1 - (1+i)^-m)/i.
fn mortgage_affordability(args: &Value) -> Result<String> {
    let budget = num(args, "monthly_budget", "mortgage_affordability")?;
    let rate = num(args, "annual_rate_percent", "mortgage_affordability")?;
    let years = num(args, "years", "mortgage_affordability")?;
    if budget <= 0.0 {
        return Err(anyhow!(
            "mortgage_affordability 'monthly_budget' must be greater than 0"
        ));
    }
    if rate < 0.0 {
        return Err(anyhow!(
            "mortgage_affordability 'annual_rate_percent' must be 0 or more"
        ));
    }
    if !(0.1..=50.0).contains(&years) {
        return Err(anyhow!(
            "mortgage_affordability 'years' must be between 0.1 and 50"
        ));
    }
    let months = (years * 12.0).round() as u64;
    let i = rate / 100.0 / 12.0;
    let principal = if i == 0.0 {
        budget * months as f64
    } else {
        budget * (1.0 - (1.0 + i).powf(-(months as f64))) / i
    };
    Ok(format!(
        "A {}/month budget at {} over {} year(s) ({} payments) affords about {} in principal",
        money(budget),
        pct(rate),
        trim_years(years),
        months,
        money(principal)
    ))
}

/// Render a year count cleanly: whole years drop the decimal, fractional keep it.
fn trim_years(years: f64) -> String {
    if (years.fract()).abs() < 1e-9 {
        format!("{}", years as i64)
    } else {
        format!("{:.2}", years)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- tip_split ---------------------------------------------------------

    #[test]
    fn tip_split_known_values() {
        // 100 bill, 20% tip, 4 people -> 20 tip, 120 total, 30 each.
        let out = tip_split(&json!({"bill": 100.0, "tip_percent": 20.0, "people": 4})).unwrap();
        assert_eq!(
            out,
            "Bill 100.00 + 20.00 tip = 120.00 total; split 4 way(s) = 30.00 each"
        );
        // Default 1 person.
        let out = tip_split(&json!({"bill": 50.0, "tip_percent": 18.0})).unwrap();
        assert_eq!(
            out,
            "Bill 50.00 + 9.00 tip = 59.00 total; split 1 way(s) = 59.00 each"
        );
    }

    #[test]
    fn tip_split_rejects_bad_args() {
        assert!(tip_split(&json!({"tip_percent": 10.0})).is_err(), "missing bill");
        assert!(tip_split(&json!({"bill": -1.0, "tip_percent": 10.0})).is_err(), "negative bill");
        assert!(tip_split(&json!({"bill": 10.0, "tip_percent": 200.0})).is_err(), "tip > 100%");
        assert!(tip_split(&json!({"bill": 10.0, "tip_percent": 10.0, "people": 0})).is_err(), "0 people");
    }

    // --- percentage_change -------------------------------------------------

    #[test]
    fn percentage_change_known_values() {
        // 100 -> 150 is +50%.
        let out = percentage_change(&json!({"old": 100.0, "new": 150.0})).unwrap();
        assert_eq!(out, "100.00 -> 150.00 is a 50.00% increase");
        // 200 -> 150 is -25%.
        let out = percentage_change(&json!({"old": 200.0, "new": 150.0})).unwrap();
        assert_eq!(out, "200.00 -> 150.00 is a -25.00% decrease");
        // Equal is no change.
        let out = percentage_change(&json!({"old": 80.0, "new": 80.0})).unwrap();
        assert_eq!(out, "80.00 -> 80.00 is a 0.00% no change");
    }

    #[test]
    fn percentage_change_negative_base_uses_abs() {
        // From -50 to -25: change of +25 over |−50| = +50%.
        let out = percentage_change(&json!({"old": -50.0, "new": -25.0})).unwrap();
        assert_eq!(out, "-50.00 -> -25.00 is a 50.00% increase");
    }

    #[test]
    fn percentage_change_rejects_zero_base() {
        assert!(percentage_change(&json!({"old": 0.0, "new": 5.0})).is_err());
        assert!(percentage_change(&json!({"new": 5.0})).is_err(), "missing old");
    }

    // --- simple_interest ---------------------------------------------------

    #[test]
    fn simple_interest_known_values() {
        // 1000 at 5% for 3 years -> 150 interest, 1150 total.
        let out = simple_interest(
            &json!({"principal": 1000.0, "annual_rate_percent": 5.0, "years": 3.0}),
        )
        .unwrap();
        assert_eq!(
            out,
            "Simple interest on 1000.00 at 5.00% for 3 year(s): 150.00 interest, 1150.00 total"
        );
    }

    #[test]
    fn simple_interest_rejects_negative() {
        assert!(simple_interest(&json!({"principal": -1.0, "annual_rate_percent": 5.0, "years": 1.0})).is_err());
        assert!(simple_interest(&json!({"principal": 1.0, "annual_rate_percent": 5.0, "years": -1.0})).is_err());
        assert!(simple_interest(&json!({"principal": 1.0, "annual_rate_percent": 5.0})).is_err(), "missing years");
    }

    // --- compound_interest -------------------------------------------------

    #[test]
    fn compound_interest_known_values() {
        // 1000 at 5% compounded annually (n=1) for 2 years -> 1000*1.05^2 = 1102.50.
        let out = compound_interest(&json!({
            "principal": 1000.0, "annual_rate_percent": 5.0, "years": 2.0, "times_per_year": 1
        }))
        .unwrap();
        assert!(out.contains("1102.50 future value"), "got: {out}");
        assert!(out.contains("102.50 interest"), "got: {out}");
    }

    #[test]
    fn compound_interest_monthly_default_beats_annual() {
        // Monthly compounding (default n=12) yields more than annual for the same APR.
        let monthly =
            compound_interest(&json!({"principal": 1000.0, "annual_rate_percent": 12.0, "years": 1.0})).unwrap();
        // 1000*(1+0.12/12)^12 = 1000*1.01^12 = 1126.825... -> 1126.83.
        assert!(monthly.contains("1126.83 future value"), "got: {monthly}");
    }

    #[test]
    fn compound_interest_rejects_bad_args() {
        assert!(compound_interest(&json!({"principal": 1.0, "annual_rate_percent": 5.0, "years": 1.0, "times_per_year": 0})).is_err());
        assert!(compound_interest(&json!({"principal": -1.0, "annual_rate_percent": 5.0, "years": 1.0})).is_err());
        assert!(compound_interest(&json!({"principal": 1.0, "years": 1.0})).is_err(), "missing rate");
    }

    // --- loan_payment ------------------------------------------------------

    #[test]
    fn loan_payment_known_values() {
        // Classic: 200000 at 6% APR over 360 months -> ~1199.10/month.
        let out = loan_payment(&json!({
            "principal": 200000.0, "annual_rate_percent": 6.0, "months": 360
        }))
        .unwrap();
        assert!(out.contains("1199.10/month"), "got: {out}");
        // Total paid = 1199.10 * 360 = 431676.38 ; interest = 231676.38.
        assert!(out.contains("431676.38 total paid"), "got: {out}");
        assert!(out.contains("231676.38 interest"), "got: {out}");
    }

    #[test]
    fn loan_payment_zero_rate_is_straight_line() {
        // 0% over 10 months on 1000 -> exactly 100/month.
        let out = loan_payment(&json!({"principal": 1000.0, "annual_rate_percent": 0.0, "months": 10})).unwrap();
        assert!(out.contains("100.00/month"), "got: {out}");
        assert!(out.contains("0.00 interest"), "got: {out}");
    }

    #[test]
    fn loan_payment_rejects_bad_args() {
        assert!(loan_payment(&json!({"principal": 0.0, "annual_rate_percent": 5.0, "months": 12})).is_err(), "zero principal");
        assert!(loan_payment(&json!({"principal": 1000.0, "annual_rate_percent": -1.0, "months": 12})).is_err(), "negative rate");
        assert!(loan_payment(&json!({"principal": 1000.0, "annual_rate_percent": 5.0, "months": 0})).is_err(), "zero months");
        assert!(loan_payment(&json!({"principal": 1000.0, "annual_rate_percent": 5.0, "months": 601})).is_err(), "months too long");
    }

    // --- roi ---------------------------------------------------------------

    #[test]
    fn roi_known_values() {
        // Cost 1000, value 1250 -> 250 profit, 25% ROI.
        let out = roi(&json!({"cost": 1000.0, "final_value": 1250.0})).unwrap();
        assert_eq!(out, "Cost 1000.00 -> value 1250.00: 250.00 profit, 25.00% ROI");
        // A loss: cost 500, value 400 -> -100, -20%.
        let out = roi(&json!({"cost": 500.0, "final_value": 400.0})).unwrap();
        assert_eq!(out, "Cost 500.00 -> value 400.00: -100.00 profit, -20.00% ROI");
    }

    #[test]
    fn roi_rejects_nonpositive_cost() {
        assert!(roi(&json!({"cost": 0.0, "final_value": 10.0})).is_err());
        assert!(roi(&json!({"cost": -5.0, "final_value": 10.0})).is_err());
        assert!(roi(&json!({"final_value": 10.0})).is_err(), "missing cost");
    }

    // --- break_even_units --------------------------------------------------

    #[test]
    fn break_even_units_known_values() {
        // Fixed 1000, price 25, variable 10 -> margin 15, exact 66.67 -> ceil 67.
        let out = break_even_units(&json!({
            "fixed_costs": 1000.0, "price_per_unit": 25.0, "variable_cost_per_unit": 10.0
        }))
        .unwrap();
        assert_eq!(out, "Contribution margin 15.00/unit; break even at 67 units (exact 66.67)");
        // Exact division -> no rounding up: fixed 100, margin 10 -> exactly 10.
        let out = break_even_units(&json!({
            "fixed_costs": 100.0, "price_per_unit": 20.0, "variable_cost_per_unit": 10.0
        }))
        .unwrap();
        assert!(out.contains("break even at 10 units"), "got: {out}");
    }

    #[test]
    fn break_even_units_rejects_nonpositive_margin() {
        // Price <= variable cost -> never breaks even.
        assert!(break_even_units(&json!({
            "fixed_costs": 1000.0, "price_per_unit": 10.0, "variable_cost_per_unit": 10.0
        }))
        .is_err());
        assert!(break_even_units(&json!({
            "fixed_costs": 1000.0, "price_per_unit": 5.0, "variable_cost_per_unit": 10.0
        }))
        .is_err());
    }

    // --- savings_goal_monthly ---------------------------------------------

    #[test]
    fn savings_goal_monthly_zero_rate_is_even_split() {
        // 12000 in 12 months, no rate -> 1000/month.
        let out = savings_goal_monthly(&json!({"goal": 12000.0, "months": 12})).unwrap();
        assert!(out.contains("save 1000.00/month"), "got: {out}");
        assert!(out.contains("at 0.00%"), "got: {out}");
    }

    #[test]
    fn savings_goal_monthly_with_rate_is_less_than_even_split() {
        // With growth, you need LESS than the flat 1000/month to hit 12000.
        let out = savings_goal_monthly(&json!({
            "goal": 12000.0, "months": 12, "annual_rate_percent": 6.0
        }))
        .unwrap();
        // i = 0.005; PMT = 12000*0.005/(1.005^12 - 1) = 60/0.0616778... = 972.80.
        assert!(out.contains("save 972.80/month"), "got: {out}");
    }

    #[test]
    fn savings_goal_monthly_rejects_bad_args() {
        assert!(savings_goal_monthly(&json!({"goal": 0.0, "months": 12})).is_err(), "zero goal");
        assert!(savings_goal_monthly(&json!({"goal": 100.0, "months": 0})).is_err(), "zero months");
        assert!(savings_goal_monthly(&json!({"goal": 100.0})).is_err(), "missing months");
        assert!(savings_goal_monthly(&json!({"goal": 100.0, "months": 12, "annual_rate_percent": -1.0})).is_err(), "negative rate");
    }

    // --- rule_of_72 --------------------------------------------------------

    #[test]
    fn rule_of_72_known_values() {
        // 8% -> 9.0 years; 6% -> 12.0 years.
        assert!(rule_of_72(&json!({"annual_rate_percent": 8.0})).unwrap().contains("9.0 years"));
        assert!(rule_of_72(&json!({"annual_rate_percent": 6.0})).unwrap().contains("12.0 years"));
    }

    #[test]
    fn rule_of_72_rejects_nonpositive_rate() {
        assert!(rule_of_72(&json!({"annual_rate_percent": 0.0})).is_err());
        assert!(rule_of_72(&json!({"annual_rate_percent": -3.0})).is_err());
        assert!(rule_of_72(&json!({})).is_err(), "missing rate");
    }

    // --- mortgage_affordability -------------------------------------------

    #[test]
    fn mortgage_affordability_round_trips_with_loan_payment() {
        // Affordability is loan_payment solved for principal: a 1199.10/month
        // budget at 6% over 30 years should afford ~200000 (the loan_payment test
        // inverse). Allow a few dollars of rounding slack.
        let out = mortgage_affordability(&json!({
            "monthly_budget": 1199.10, "annual_rate_percent": 6.0, "years": 30.0
        }))
        .unwrap();
        assert!(out.contains("360 payments"), "got: {out}");
        // Extract the principal from the tail "affords about X in principal".
        let tail = out.split("affords about ").nth(1).unwrap();
        let amount: f64 = tail.split(" in principal").next().unwrap().parse().unwrap();
        assert!((amount - 200000.0).abs() < 5.0, "expected ~200000, got {amount}");
    }

    #[test]
    fn mortgage_affordability_zero_rate_is_budget_times_months() {
        // 0% over 1 year: 12 payments of 500 -> 6000 principal.
        let out = mortgage_affordability(&json!({
            "monthly_budget": 500.0, "annual_rate_percent": 0.0, "years": 1.0
        }))
        .unwrap();
        assert!(out.contains("6000.00 in principal"), "got: {out}");
    }

    #[test]
    fn mortgage_affordability_rejects_bad_args() {
        assert!(mortgage_affordability(&json!({"monthly_budget": 0.0, "annual_rate_percent": 5.0, "years": 30.0})).is_err());
        assert!(mortgage_affordability(&json!({"monthly_budget": 500.0, "annual_rate_percent": -1.0, "years": 30.0})).is_err());
        assert!(mortgage_affordability(&json!({"monthly_budget": 500.0, "annual_rate_percent": 5.0, "years": 100.0})).is_err(), "term too long");
    }

    // --- catalog -----------------------------------------------------------

    #[test]
    fn catalog_is_all_pure_and_has_the_expected_skills() {
        let s = skills();
        let names: Vec<&str> = s.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "tip_split",
                "percentage_change",
                "simple_interest",
                "compound_interest",
                "loan_payment",
                "roi",
                "break_even_units",
                "savings_goal_monthly",
                "rule_of_72",
                "mortgage_affordability",
            ]
        );
        // Every finance skill is PURE: not consequential, not source-gated, and
        // lives in the Finance category with a non-empty description + cues.
        assert!(s.iter().all(|d| !d.consequential && !d.source_gated));
        assert!(s.iter().all(|d| d.category == Category::Finance));
        assert!(s.iter().all(|d| !d.description.is_empty() && !d.cues.is_empty()));
    }

    #[test]
    fn every_skill_run_is_deterministic() {
        // Same args -> identical output, every call (the hermetic-testability
        // contract). Spot-check the formula skills.
        let cases: Vec<(super::super::RunFn, Value)> = vec![
            (tip_split, json!({"bill": 73.21, "tip_percent": 17.5, "people": 3})),
            (compound_interest, json!({"principal": 2500.0, "annual_rate_percent": 4.3, "years": 7.0})),
            (loan_payment, json!({"principal": 18000.0, "annual_rate_percent": 4.9, "months": 60})),
            (savings_goal_monthly, json!({"goal": 50000.0, "months": 84, "annual_rate_percent": 5.5})),
        ];
        for (f, args) in cases {
            let a = f(&args).unwrap();
            let b = f(&args).unwrap();
            assert_eq!(a, b, "deterministic for {args}");
        }
    }
}
