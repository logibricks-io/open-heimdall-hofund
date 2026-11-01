use serde_json::Value;

use super::{BalancePart, ProviderError, Wallet, currency_unit, money};

pub(super) fn read(
    body: &[u8],
    path: &str,
) -> Result<(String, Vec<BalancePart>, Vec<Wallet>), ProviderError> {
    let reply: Value =
        serde_json::from_slice(body).map_err(|_| ProviderError::decode("the reply is not JSON"))?;
    if !path.contains(';') && !path.contains(':') {
        let (part, wallet) = read_one(&reply, path)?;
        return Ok((part.text.clone(), vec![part], wallet.into_iter().collect()));
    }
    let mut parts = Vec::new();
    let mut wallets = Vec::new();
    let mut first_error = None;
    let mut read_any = false;
    for expression in path.split(';') {
        if expression.trim().is_empty() {
            continue;
        }
        let (label, expression) = expression
            .split_once(':')
            .map_or(("", expression), |(label, expression)| {
                (label.trim(), expression)
            });
        match read_one(&reply, expression) {
            Ok((mut part, wallet)) => {
                part.label = label.to_owned();
                read_any = true;
                if let Some(wallet) = wallet {
                    wallets.push(Wallet {
                        name: if label.is_empty() {
                            wallet.name
                        } else {
                            label.to_owned()
                        },
                        ..wallet
                    });
                }
                parts.push(part);
            }
            Err(error) => {
                let message = if label.is_empty() {
                    error.message
                } else {
                    format!("{label}: {}", error.message)
                };
                if first_error.is_none() {
                    first_error = Some(message.clone());
                }
                parts.push(BalancePart {
                    label: label.to_owned(),
                    text: "—".to_owned(),
                    percent: None,
                    error: message,
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(ProviderError::decode(
            "no balance path: where in the reply the amount is, e.g. data.balance",
        ));
    }
    if !read_any {
        return Err(ProviderError::decode(
            first_error.unwrap_or_else(|| "no balance in the reply".to_owned()),
        ));
    }
    let text = parts
        .iter()
        .map(|part| {
            if part.label.is_empty() {
                part.text.clone()
            } else {
                format!("{} {}", part.label, part.text)
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    Ok((text, parts, wallets))
}

fn read_one(reply: &Value, path: &str) -> Result<(BalancePart, Option<Wallet>), ProviderError> {
    let mut path = path.trim();
    let mut sign = "";
    for candidate in ["$", "¥", "€", "£"] {
        if let Some(rest) = path.strip_prefix(candidate) {
            sign = candidate;
            path = rest.trim_start();
            break;
        }
    }
    let percent = path.ends_with('%');
    if percent {
        path = path[..path.len() - 1].trim_end();
    }
    if path.is_empty() {
        return Err(ProviderError::decode(
            "no balance path: where in the reply the amount is, e.g. data.balance",
        ));
    }
    let expression = Expression {
        source: path,
        index: 0,
        reply,
    };
    if expression.lone() {
        let value = expression.at(path)?;
        if let Some(number) = number(&value) {
            let part = amount_part(sign, number, percent);
            let wallet = (!percent).then(|| Wallet {
                name: currency_unit(sign).to_owned(),
                amount: number,
                unit: currency_unit(sign).to_owned(),
            });
            return Ok((part, wallet));
        }
        if let Value::String(text) = value
            && !text.is_empty()
            && !percent
        {
            return Ok((
                BalancePart {
                    text: format!("{sign}{text}"),
                    ..BalancePart::default()
                },
                None,
            ));
        }
        return Err(ProviderError::decode(format!(
            "{path:?} in the reply is not an amount"
        )));
    }
    let value = expression.number()?;
    if !value.is_finite() {
        return Err(ProviderError::decode("the balance path divides by nothing"));
    }
    let part = amount_part(sign, value, percent);
    let wallet = (!percent).then(|| Wallet {
        name: currency_unit(sign).to_owned(),
        amount: value,
        unit: currency_unit(sign).to_owned(),
    });
    Ok((part, wallet))
}

fn amount_part(sign: &str, value: f64, percent: bool) -> BalancePart {
    if percent {
        let mut text = format!("{:.1}", value * 100.0);
        if text.ends_with(".0") {
            text.truncate(text.len() - 2);
        }
        BalancePart {
            text: format!("{sign}{text}%"),
            percent: Some(value * 100.0),
            ..BalancePart::default()
        }
    } else {
        BalancePart {
            text: money(sign, value),
            ..BalancePart::default()
        }
    }
}

#[derive(Clone, Default)]
struct Eval {
    number: f64,
    numbers: Vec<f64>,
    many: bool,
    items: Vec<Value>,
}

#[derive(Clone)]
struct Expression<'a> {
    source: &'a str,
    index: usize,
    reply: &'a Value,
}

impl Expression<'_> {
    fn lone(&self) -> bool {
        !self.source.is_empty()
            && !self.source.as_bytes()[0].is_ascii_digit()
            && !self
                .source
                .bytes()
                .any(|byte| matches!(byte, b'+' | b'-' | b'*' | b'/' | b'(' | b')' | b' ' | b'['))
    }

    fn number(mut self) -> Result<f64, ProviderError> {
        let value = self.sum()?;
        if self.index < self.source.len() {
            return Err(ProviderError::decode(format!(
                "the balance path has {:?} it can't read",
                &self.source[self.index..]
            )));
        }
        if value.many {
            return Err(ProviderError::decode(
                "the balance path has several values where one is wanted: put them in sum(), max(), min() or count()",
            ));
        }
        Ok(value.number)
    }

    fn sum(&mut self) -> Result<Eval, ProviderError> {
        let mut value = self.product()?;
        loop {
            self.space();
            let Some(operator) = self.source.as_bytes().get(self.index).copied() else {
                break;
            };
            if !matches!(operator, b'+' | b'-') {
                break;
            }
            self.index += 1;
            let right = self.product()?;
            value = operate(value, right, operator)?;
        }
        Ok(value)
    }

    fn product(&mut self) -> Result<Eval, ProviderError> {
        let mut value = self.unary()?;
        loop {
            self.space();
            let Some(operator) = self.source.as_bytes().get(self.index).copied() else {
                break;
            };
            if !matches!(operator, b'*' | b'/') {
                break;
            }
            self.index += 1;
            let right = self.unary()?;
            value = operate(value, right, operator)?;
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<Eval, ProviderError> {
        self.space();
        let Some(byte) = self.source.as_bytes().get(self.index).copied() else {
            return Err(ProviderError::decode(
                "the balance path ends where a number or a path should be",
            ));
        };
        match byte {
            b'-' => {
                self.index += 1;
                let value = self.unary()?;
                operate(Eval::default(), value, b'-')
            }
            b'(' => {
                self.index += 1;
                let value = self.sum()?;
                self.closed(value)
            }
            b'0'..=b'9' | b'.' => {
                let start = self.index;
                while self
                    .source
                    .as_bytes()
                    .get(self.index)
                    .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
                {
                    self.index += 1;
                }
                let number = self.source[start..self.index].parse().map_err(|_| {
                    ProviderError::decode(format!(
                        "the balance path has {:?}, not a number",
                        &self.source[start..self.index]
                    ))
                })?;
                Ok(Eval {
                    number,
                    ..Eval::default()
                })
            }
            _ => {
                let start = self.index;
                while let Some(byte) = self.source.as_bytes().get(self.index).copied() {
                    if byte == b'[' {
                        let Some(close) = self.source[self.index..].find(']') else {
                            return Err(ProviderError::decode(
                                "the balance path has a [ without its ]",
                            ));
                        };
                        self.index += close + 1;
                        continue;
                    }
                    if byte == b'*'
                        && (self.index == start
                            || self.source.as_bytes().get(self.index - 1) == Some(&b'.'))
                    {
                        self.index += 1;
                        continue;
                    }
                    if matches!(byte, b'+' | b'-' | b'*' | b'/' | b'(' | b')' | b' ') {
                        break;
                    }
                    self.index += 1;
                }
                if self.index == start {
                    return Err(ProviderError::decode(format!(
                        "the balance path has {:?} where a number or a path should be",
                        &self.source[start..]
                    )));
                }
                let path = &self.source[start..self.index];
                if matches!(path, "sum" | "count" | "max" | "min")
                    && self.source.as_bytes().get(self.index) == Some(&b'(')
                {
                    self.index += 1;
                    let value = self.sum()?;
                    let value = self.closed(value)?;
                    return aggregate(path, value);
                }
                let (values, many) = self.all(path)?;
                if !many {
                    let number = values.first().and_then(number).ok_or_else(|| {
                        ProviderError::decode(format!("{path:?} in the reply is not a number"))
                    })?;
                    return Ok(Eval {
                        number,
                        ..Eval::default()
                    });
                }
                let numbers = values
                    .iter()
                    .map(|value| number(value).unwrap_or(f64::NAN))
                    .collect();
                Ok(Eval {
                    numbers,
                    many: true,
                    items: values,
                    ..Eval::default()
                })
            }
        }
    }

    fn closed(&mut self, value: Eval) -> Result<Eval, ProviderError> {
        self.space();
        if self.source.as_bytes().get(self.index) != Some(&b')') {
            return Err(ProviderError::decode(
                "the balance path has a ( without its )",
            ));
        }
        self.index += 1;
        Ok(value)
    }

    fn at(&self, path: &str) -> Result<Value, ProviderError> {
        let (values, many) = self.all(path)?;
        if many {
            return Err(ProviderError::decode(format!(
                "{path:?} is several values: put it in sum(), max(), min() or count()"
            )));
        }
        values
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::decode(format!("nothing at {path:?} in the reply")))
    }

    fn all(&self, path: &str) -> Result<(Vec<Value>, bool), ProviderError> {
        let segments = segments(path)?;
        let mut values = vec![self.reply.clone()];
        let mut many = false;
        for segment in segments {
            let mut next = Vec::new();
            for value in &values {
                if value.is_null() {
                    next.push(Value::Null);
                } else {
                    next.extend(segment.step(value));
                }
            }
            many |= segment.spreads();
            values = next;
            if !many && (values.len() != 1 || values[0].is_null()) {
                return Err(ProviderError::decode(format!(
                    "nothing at {path:?} in the reply"
                )));
            }
        }
        Ok((values, many))
    }

    fn space(&mut self) {
        while self.source.as_bytes().get(self.index) == Some(&b' ') {
            self.index += 1;
        }
    }
}

fn operate(left: Eval, right: Eval, operator: u8) -> Result<Eval, ProviderError> {
    fn one(left: f64, right: f64, operator: u8) -> f64 {
        match operator {
            b'+' => left + right,
            b'-' => left - right,
            b'*' => left * right,
            _ if right == 0.0 => f64::NAN,
            _ => left / right,
        }
    }
    if !left.many && !right.many {
        if operator == b'/' && right.number == 0.0 {
            return Err(ProviderError::decode("the balance path divides by 0"));
        }
        return Ok(Eval {
            number: one(left.number, right.number, operator),
            ..Eval::default()
        });
    }
    if left.many && right.many && left.numbers.len() != right.numbers.len() {
        return Err(ProviderError::decode(format!(
            "the balance path puts {} values with {}",
            left.numbers.len(),
            right.numbers.len()
        )));
    }
    let size = left.numbers.len().max(right.numbers.len());
    let numbers = (0..size)
        .map(|index| {
            let a = if left.many {
                left.numbers[index]
            } else {
                left.number
            };
            let b = if right.many {
                right.numbers[index]
            } else {
                right.number
            };
            one(a, b, operator)
        })
        .collect();
    Ok(Eval {
        numbers,
        many: true,
        ..Eval::default()
    })
}

fn aggregate(name: &str, value: Eval) -> Result<Eval, ProviderError> {
    let numbers = if value.many {
        value.numbers
    } else {
        vec![value.number]
    };
    let number = match name {
        "sum" => numbers
            .iter()
            .filter(|number| !number.is_nan())
            .sum::<f64>(),
        "count" => {
            if value.many && !value.items.is_empty() {
                value.items.iter().filter(|value| !value.is_null()).count() as f64
            } else {
                numbers.iter().filter(|number| !number.is_nan()).count() as f64
            }
        }
        "max" | "min" => {
            let direction = if name == "max" { 1.0 } else { -1.0 };
            let mut best = None;
            for number in numbers.into_iter().filter(|number| !number.is_nan()) {
                if best.is_none_or(|old| (number - old) * direction > 0.0) {
                    best = Some(number);
                }
            }
            best.ok_or_else(|| {
                ProviderError::decode(format!(
                    "{name}() of the balance path has nothing to take it of"
                ))
            })?
        }
        _ => unreachable!(),
    };
    Ok(Eval {
        number,
        ..Eval::default()
    })
}

#[derive(Clone)]
struct Segment {
    key: String,
    tests: Vec<Test>,
}

#[derive(Clone)]
struct Test {
    path: Vec<String>,
    not: bool,
    want: String,
}

impl Segment {
    fn spreads(&self) -> bool {
        self.key == "*" || !self.tests.is_empty()
    }

    fn step(&self, value: &Value) -> Vec<Value> {
        let value = if self.key == "*" {
            Value::Array(items(value))
        } else if self.key.is_empty() {
            value.clone()
        } else {
            child(value, &self.key).cloned().unwrap_or(Value::Null)
        };
        if self.tests.is_empty() {
            return if self.key == "*" {
                value.as_array().cloned().unwrap_or_default()
            } else {
                vec![value]
            };
        }
        let candidates = value.as_array().cloned().unwrap_or_else(|| vec![value]);
        candidates
            .into_iter()
            .filter(|value| !value.is_null() && self.passes(value))
            .collect()
    }

    fn passes(&self, value: &Value) -> bool {
        self.tests.iter().all(|test| {
            let mut found = value;
            for key in &test.path {
                found = child(found, key).unwrap_or(&Value::Null);
            }
            let got = match found {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => String::new(),
            };
            (!found.is_null() && got == test.want) != test.not
        })
    }
}

fn child<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => object.get(key),
        Value::Array(array) => key.parse::<usize>().ok().and_then(|index| array.get(index)),
        _ => None,
    }
}

fn items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(array) => array.clone(),
        Value::Object(object) => object.values().cloned().collect(),
        _ => Vec::new(),
    }
}

fn segments(path: &str) -> Result<Vec<Segment>, ProviderError> {
    let mut out = Vec::new();
    let mut rest = path;
    loop {
        let mut end = 0;
        while end < rest.len() && rest.as_bytes()[end] != b'.' {
            if rest.as_bytes()[end] == b'[' {
                let Some(close) = rest[end..].find(']') else {
                    return Err(ProviderError::decode(
                        "the balance path has a [ without its ]",
                    ));
                };
                end += close;
            }
            end += 1;
        }
        out.push(segment(&rest[..end], path)?);
        if end == rest.len() {
            return Ok(out);
        }
        rest = &rest[end + 1..];
    }
}

fn segment(raw: &str, path: &str) -> Result<Segment, ProviderError> {
    let (key, mut tests) = raw.split_once('[').unwrap_or((raw, ""));
    if key.is_empty() && tests.is_empty() {
        return Err(ProviderError::decode(format!(
            "nothing at {path:?} in the reply"
        )));
    }
    let mut parsed = Vec::new();
    while !tests.is_empty() {
        let Some((body, after)) = tests.split_once(']') else {
            return Err(ProviderError::decode(
                "the balance path has a [ without its ]",
            ));
        };
        let Some((left, want)) = body.split_once('=') else {
            return Err(ProviderError::decode(format!(
                "the balance path has [{body}], not a test like [status=active]"
            )));
        };
        if left.trim().is_empty() {
            return Err(ProviderError::decode(format!(
                "the balance path has [{body}], not a test like [status=active]"
            )));
        }
        let (left, not) = left
            .strip_suffix('!')
            .map_or((left, false), |left| (left, true));
        parsed.push(Test {
            path: left.trim().split('.').map(str::to_owned).collect(),
            not,
            want: want.trim().trim_matches(['\'', '"']).to_owned(),
        });
        tests = after.strip_prefix('[').unwrap_or(after);
    }
    Ok(Segment {
        key: key.to_owned(),
        tests: parsed,
    })
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}
