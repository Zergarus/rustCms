//! Форматирование чисел для шаблонов админки.

use std::path::Path;

use minijinja::Environment;

fn grouped(digits: &str) -> String {
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Деньги: `6 897,20`, знак минус перед числом.
pub fn money(value: f64) -> String {
    let cents = (value * 100.0).round() as i64;
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.abs();
    format!(
        "{sign}{},{:02}",
        grouped(&(cents / 100).to_string()),
        cents % 100
    )
}

/// Числа без хвостовых нулей: `23 119`, `2,5`.
pub fn num(value: f64) -> String {
    if !value.is_finite() {
        return String::new();
    }
    if (value.fract()).abs() < 1e-9 {
        return grouped(&(value as i64).to_string());
    }
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    let int = int.trim_start_matches('-');
    let sign = if value < 0.0 { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{}", grouped(int))
    } else {
        format!("{sign}{},{}", grouped(int), frac.replace('.', ","))
    }
}

/// Окружение шаблонов админки: загрузчик и фильтры `money`/`num`.
pub fn templates_env(dir: &Path) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_loader(minijinja::path_loader(dir));
    env.add_filter("money", money);
    env.add_filter("num", num);
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formats_rubles() {
        assert_eq!(money(6897.2), "6 897,20");
        assert_eq!(money(1394.22), "1 394,22");
        assert_eq!(money(0.0), "0,00");
        assert_eq!(money(-5.5), "-5,50");
    }

    #[test]
    fn num_hides_zero_fraction() {
        assert_eq!(num(23119.0), "23 119");
        assert_eq!(num(2.5), "2,5");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(-1200.0), "-1 200");
    }
}
