//! Применение правил работы с корзиной (как `Sale\DiscountBase::executeDiscountList` и
//! `Sale\Discount\Actions` Битрикса, режим расчёта «каждое правило от текущей цены»).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::cond::{self, State};
use super::context::{Context, CtxItem};
use super::cumulative;
use super::model::{
    Coupon, CouponKind, GiftOffer, GiftType, Node, Rule, as_num, gift_spec, round2,
};
use crate::bxapi::project::DiscountConditionHook;
use crate::catalog::rounding::Rounding;

/// Итог по позиции.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemResult {
    /// Цена единицы после скидок.
    pub price: f64,
    /// `base_price - price` на единицу (у наценки — отрицательная).
    pub discount: f64,
    /// Применённые к позиции правила по порядку.
    pub rules: Vec<i64>,
}

/// Сработавшее правило.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub discount_id: i64,
    pub name: String,
    pub coupon: Option<String>,
    /// Позиция → сумма скидки по позиции (на всё количество; у наценки — отрицательная).
    pub items: Vec<(i64, f64)>,
    /// Скидка на доставку (у наценки — отрицательная).
    pub delivery: f64,
    /// Позиции и доставка вместе.
    pub sum: f64,
}

/// Статус купона покупателя (`DiscountCouponsManager`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CouponStatus {
    Applied,
    /// Купон годен, но правило не сработало.
    NotApplied,
    NotFound,
    Inactive,
    Expired,
    Used,
    Foreign,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiscountResult {
    pub items: HashMap<i64, ItemResult>,
    /// Цена доставки после скидок; `None` — доставки в контексте нет.
    pub delivery_price: Option<f64>,
    pub applied: Vec<Applied>,
    pub coupons: Vec<(String, CouponStatus)>,
    /// Доступные подарки: правила с подарком, дошедшие до проверки условий и с выполненными
    /// условиями, без уже подаренного (`Gift\Manager::getCollectionsByBasket`).
    pub gifts: Vec<GiftOffer>,
}

type Hook<'a> = Option<&'a dyn DiscountConditionHook>;

/// `Sale\Discount\Actions::VALUE_EPS`.
const EPS: f64 = 1e-5;

/// `Actions::roundZeroValue`.
fn round_zero(x: f64) -> f64 {
    if x.abs() <= EPS { 0.0 } else { x }
}

/// Период активности: начало в будущем — не активно, конец в прошлом — истекло.
fn period(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<CouponStatus> {
    if from.is_some_and(|f| f > now) {
        return Some(CouponStatus::Inactive);
    }
    if to.is_some_and(|t| t < now) {
        return Some(CouponStatus::Expired);
    }
    None
}

/// Проверки `DiscountCouponsManager::checkBaseData`/`checkFullData`: `None` — купон годен.
fn coupon_error(cp: &Coupon, rules: &[&Rule], ctx: &Context) -> Option<CouponStatus> {
    if !cp.active {
        return Some(CouponStatus::Inactive);
    }
    if let Some(st) = period(cp.active_from, cp.active_to, ctx.now) {
        return Some(st);
    }
    let Some(rule) = rules.iter().find(|r| r.id == cp.discount_id) else {
        return Some(CouponStatus::Inactive);
    };
    if !rule.active {
        return Some(CouponStatus::Inactive);
    }
    if let Some(st) = period(rule.active_from, rule.active_to, ctx.now) {
        return Some(st);
    }
    if cp.user_id.is_some_and(|u| u > 0 && Some(u) != ctx.user_id) {
        return Some(CouponStatus::Foreign);
    }
    if cp.max_use > 0 && cp.use_count >= cp.max_use {
        return Some(CouponStatus::Used);
    }
    None
}

/// Правило действует для покупателя (без купонов): активно, в датах, по группам, без
/// неподдерживаемых классов.
fn rule_active(rule: &Rule, ctx: &Context) -> bool {
    rule.active
        && period(rule.active_from, rule.active_to, ctx.now).is_none()
        && (rule.group_ids.is_empty() || rule.group_ids.iter().any(|g| ctx.group_ids.contains(g)))
        && rule.unsupported.trim().is_empty()
}

/// Изменяемое состояние расчёта: текущие цены, применённые к позициям правила, доставка.
struct Calc<'a> {
    ctx: &'a Context,
    hook: Hook<'a>,
    prices: HashMap<i64, f64>,
    applied: HashMap<i64, Vec<i64>>,
    delivery: Option<f64>,
    /// Позиции, подаренные действиями текущего правила.
    gifted: Vec<i64>,
}

impl Calc<'_> {
    fn price(&self, key: i64) -> f64 {
        self.prices.get(&key).copied().unwrap_or(0.0)
    }

    fn check_order(&self, node: &Node) -> bool {
        let st = State {
            ctx: self.ctx,
            prices: &self.prices,
            applied: &self.applied,
        };
        cond::check_order(node, &st, self.hook)
    }

    /// Действия правила (`CSaleActionCtrlGroup`: дети корня по порядку).
    fn run_actions(&mut self, actions: &Node) {
        for act in &actions.children {
            match act.class_id.as_str() {
                "ActSaleBsktGrp" => self.apply_basket(act),
                "ActSaleDelivery" => self.apply_delivery(act),
                "CumulativeAction" => self.apply_cumulative(act),
                "GiftCondGroup" => self.apply_gift(act),
                _ => {}
            }
        }
    }

    /// `ActSaleBsktGrp::Generate` + `Actions::applyToBasket`.
    fn apply_basket(&mut self, node: &Node) {
        let d = &node.data;
        let (Some(kind), Some(value), Some(unit)) = (
            d.get("Type").and_then(Value::as_str),
            d.get("Value").and_then(as_num),
            d.get("Unit").and_then(Value::as_str),
        ) else {
            return;
        };
        let max = d.get("Max").and_then(as_num).unwrap_or(0.0).trunc();
        let (value, limit) = match (kind, unit) {
            ("Discount", _) => (-value, max),
            ("Extra", _) => (value, 0.0),
            ("Closeout", "CurEach") => (value, 0.0),
            _ => return,
        };
        if !matches!(unit, "Perc" | "CurEach" | "CurAll") {
            return;
        }
        let rows: Vec<(i64, f64)> = {
            let st = State {
                ctx: self.ctx,
                prices: &self.prices,
                applied: &self.applied,
            };
            self.ctx
                .items
                .iter()
                .filter(|it| !it.custom_price && cond::check_item(node, it, &st, self.hook))
                .map(|it| (it.key, it.quantity))
                .collect()
        };
        if rows.is_empty() {
            return;
        }
        let value = round_zero(value);
        if value == 0.0 {
            return;
        }
        // Изменение цены единицы по позициям
        let deltas: Vec<f64> = match unit {
            "CurAll" => {
                let Some(d) = self.cur_all(&rows, value) else {
                    return;
                };
                d
            }
            "Perc" => rows
                .iter()
                .map(|&(k, _)| round2(self.price(k) * value / 100.0))
                .collect(),
            _ => vec![round2(value); rows.len()],
        };
        let closeout = kind == "Closeout";
        for (&(key, _), &calc) in rows.iter().zip(&deltas) {
            let price = self.price(key);
            let result = if closeout {
                // `VALUE_TYPE_CLOSEOUT`: только если ниже текущей
                if calc < price { calc } else { continue }
            } else {
                let calc = if unit != "CurAll" && limit > 0.0 && limit + calc <= 0.0 {
                    -limit
                } else {
                    calc
                };
                round_zero(price + calc)
            };
            // Цена ниже нуля — к позиции не применяется (`if ($result >= 0)`)
            if result < 0.0 {
                continue;
            }
            self.prices.insert(key, round2(result));
        }
    }

    /// `CurAll` (`VALUE_TYPE_SUMM`): сумма — процент от текущей стоимости позиций
    /// (`getPercentByValue`), на единицу с округлением, как Битрикс. Затем расхождение с
    /// `value` (в копейках) забирают позиции по возрастанию количества (из равных — с конца):
    /// каждая — целыми копейками на единицу, не ниже нуля по цене; если остаток так не делится,
    /// первая из них берёт ближайшую копейку на единицу.
    fn cur_all(&self, rows: &[(i64, f64)], value: f64) -> Option<Vec<f64>> {
        let sum: f64 = rows.iter().map(|&(k, q)| self.price(k) * q).sum();
        let percent = round_zero(if sum > 0.0 { value * 100.0 / sum } else { 0.0 });
        // Скидка — процент в [-100, 0), наценка — больше нуля
        if (value < 0.0 && !(-100.0..0.0).contains(&percent)) || (value > 0.0 && percent <= 0.0) {
            return None;
        }
        let cents = |x: f64| (round2(x) * 100.0).round() as i64;
        // Копейки на единицу по позициям
        let mut units: Vec<i64> = rows
            .iter()
            .map(|&(k, _)| cents(self.price(k) * percent / 100.0))
            .collect();
        let row_total = |units: &[i64]| -> i64 {
            rows.iter()
                .zip(units)
                .map(|(&(_, q), &u)| cents(u as f64 * q / 100.0))
                .sum()
        };
        let target = cents(value);
        // Изменение на единицу не выводит цену ниже нуля и не меняет знак действия
        let clamp = |i: usize, u: i64| -> i64 {
            if value < 0.0 {
                u.clamp(-cents(self.price(rows[i].0)), 0)
            } else {
                u.max(0)
            }
        };
        let mut order: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].1 > 0.0).collect();
        order.sort_by(|&a, &b| rows[a].1.total_cmp(&rows[b].1).then(b.cmp(&a)));
        for &i in &order {
            let diff = target - row_total(&units);
            if diff == 0 {
                break;
            }
            let adj = (diff as f64 / rows[i].1).trunc() as i64;
            units[i] = clamp(i, units[i] + adj);
        }
        let diff = target - row_total(&units);
        if diff != 0
            && let Some(&i) = order.first()
        {
            let adj = (diff as f64 / rows[i].1).round() as i64;
            units[i] = clamp(i, units[i] + adj);
        }
        Some(units.into_iter().map(|u| u as f64 / 100.0).collect())
    }

    /// `CSaleCumulativeAction::Generate` + `Actions::applyCumulativeToBasket`: пороги по `sum`,
    /// действует последний с `накоплено >= sum`; скидка `P` — процент от текущей цены, `F` —
    /// сумма на единицу. Позиции — по фильтру детей (`All`/`True`, как `buildSubsCode`).
    fn apply_cumulative(&mut self, node: &Node) {
        let d = &node.data;
        let Some(ranges) = d.get("ranges").and_then(Value::as_array) else {
            return;
        };
        let mut ranges: Vec<(f64, f64, &str)> = ranges
            .iter()
            .filter_map(|r| {
                Some((
                    as_num(r.get("sum")?)?,
                    as_num(r.get("value")?)?,
                    r.get("type")?.as_str()?,
                ))
            })
            .collect();
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total = self
            .ctx
            .cumulative
            .get(&cumulative::from_action(d))
            .copied()
            .unwrap_or(0.0);
        let Some(&(_, value, unit)) = ranges.iter().rfind(|r| total >= r.0) else {
            return;
        };
        if !matches!(unit, "P" | "F") {
            return;
        }
        let value = round_zero(-value);
        if value == 0.0 {
            return;
        }
        let more_profitable =
            d.get("apply_if_more_profitable").and_then(Value::as_str) == Some("Y");
        // Отклонение от `wrapFilterToFindMoreProfitableForCumulative`: у Битрикса позиция без
        // скидок проходит мимо фильтра детей, у нас фильтр проверяется всегда.
        let rows: Vec<(i64, f64)> = {
            let st = State {
                ctx: self.ctx,
                prices: &self.prices,
                applied: &self.applied,
            };
            self.ctx
                .items
                .iter()
                .filter(|it| !it.custom_price && cond::check_item(node, it, &st, self.hook))
                .map(|it| (it.key, it.base_price))
                .collect()
        };
        for (key, base) in rows {
            let current = self.price(key);
            // «Только если выгоднее»: от базовой цены, предыдущие скидки позиции заменяются
            let from = if more_profitable { base } else { current };
            let calc = if unit == "P" {
                round2(from * value / 100.0)
            } else {
                round2(value)
            };
            let result = round_zero(from + calc);
            if result < 0.0 || (more_profitable && result >= current) {
                continue;
            }
            self.prices.insert(key, round2(result));
        }
    }

    /// `CSaleActionGiftCtrlGroup::Generate` + `Actions::applySimpleGift`: позиции по возрастанию
    /// текущей цены; подходит товар из списка (у предложения — и родитель) или из раздела, в
    /// количестве ровно 1, с ценой > 0 и без ручной цены (`GenerateApplyCallableFilter`,
    /// `filterBasketForAction`). `one` — первая подходящая, `all` — все; цена становится 0.
    fn apply_gift(&mut self, node: &Node) {
        let Some((kind, products, section)) = gift_spec(node) else {
            return;
        };
        let matches = |it: &CtxItem| match section {
            Some(s) => it.section_ids.contains(&s),
            None => {
                products.contains(&it.product_id)
                    || it.parent.as_ref().is_some_and(|p| products.contains(&p.id))
            }
        };
        let mut rows: Vec<(i64, f64)> = self
            .ctx
            .items
            .iter()
            .filter(|it| {
                it.quantity == 1.0 && !it.custom_price && self.price(it.key) > 0.0 && matches(it)
            })
            .map(|it| (it.key, self.price(it.key)))
            .collect();
        rows.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        if kind == GiftType::One {
            rows.truncate(1);
        }
        for (key, _) in rows {
            self.prices.insert(key, 0.0);
            self.gifted.push(key);
        }
    }

    /// `ActSaleDelivery::Generate` + `Actions::applyToDelivery`.
    fn apply_delivery(&mut self, node: &Node) {
        let d = &node.data;
        let (Some(kind), Some(value), Some(unit)) = (
            d.get("Type").and_then(Value::as_str),
            d.get("Value").and_then(as_num),
            d.get("Unit").and_then(Value::as_str),
        ) else {
            return;
        };
        let value = match kind {
            "Extra" => value,
            "Discount" | "DiscountZero" => -value,
            _ => return,
        };
        // «Скидку не более» в валюте — до нуля (`MAX_BOUND`)
        let max_bound = kind == "DiscountZero" && unit == "Cur";
        let Some(price) = self.delivery else {
            return;
        };
        let value = match unit {
            "Perc" => price * value / 100.0,
            "Cur" => value,
            _ => return,
        };
        if round_zero(price) == 0.0 && kind != "Extra" {
            return;
        }
        let value = round_zero(round2(value));
        if value == 0.0 {
            return;
        }
        let mut result = round_zero(price + value);
        if max_bound && result < 0.0 {
            result = 0.0;
        }
        if result < 0.0 {
            return;
        }
        self.delivery = Some(round2(result));
    }
}

pub fn calculate(rules: &[Rule], ctx: &Context, hook: Hook) -> DiscountResult {
    let refs: Vec<&Rule> = rules.iter().collect();
    calculate_refs(&refs, ctx, hook)
}

fn calculate_refs(rules: &[&Rule], ctx: &Context, hook: Hook) -> DiscountResult {
    let mut calc = Calc {
        ctx,
        hook,
        prices: ctx.items.iter().map(|it| (it.key, it.base_price)).collect(),
        applied: HashMap::new(),
        delivery: ctx.order.delivery_price,
        gifted: Vec::new(),
    };

    let mut coupons: Vec<CouponStatus> = ctx
        .coupons
        .iter()
        .map(|cp| coupon_error(cp, rules, ctx).unwrap_or(CouponStatus::NotApplied))
        .collect();

    // Подходящие правила; у правила по купону — первый годный купон покупателя
    let mut list: Vec<(&Rule, Option<usize>)> = rules
        .iter()
        .copied()
        .filter(|r| rule_active(r, ctx))
        .filter_map(|r| {
            if !r.use_coupons {
                return Some((r, None));
            }
            ctx.coupons
                .iter()
                .enumerate()
                .find(|(i, cp)| cp.discount_id == r.id && coupons[*i] == CouponStatus::NotApplied)
                .map(|(i, _)| (r, Some(i)))
        })
        .collect();
    list.sort_by_key(|(r, _)| (std::cmp::Reverse(r.priority), r.sort, r.id));

    let mut applied = Vec::new();
    let mut gifts = Vec::new();
    let mut skip_level: Option<i32> = None;
    for (rule, coupon) in list {
        if skip_level == Some(rule.priority) {
            continue;
        }
        skip_level = None;
        if !calc.check_order(&rule.conditions) {
            continue;
        }
        let old_prices = calc.prices.clone();
        let old_delivery = calc.delivery;
        calc.gifted.clear();
        calc.run_actions(&rule.actions);

        let mut changed: Vec<(i64, f64)> = ctx
            .items
            .iter()
            .filter(|it| calc.prices[&it.key] != old_prices[&it.key])
            .map(|it| (it.key, it.quantity))
            .collect();
        // Купон «на одну позицию»: скидка остаётся на самой дорогой (`correctStepResult`)
        let coupon_data = coupon.map(|i| &ctx.coupons[i]);
        if coupon_data.is_some_and(|cp| cp.kind == CouponKind::Basket) && changed.len() > 1 {
            let mut keep = changed[0].0;
            for &(key, _) in &changed {
                if old_prices[&key] > old_prices[&keep] {
                    keep = key;
                }
            }
            for &(key, _) in &changed {
                if key != keep {
                    calc.prices.insert(key, old_prices[&key]);
                }
            }
            changed.retain(|&(key, _)| key == keep);
        }
        // Подаренное правилом (у предложения — и родитель, `ExtendProductIds`)
        let gifted: Vec<i64> = ctx
            .items
            .iter()
            .filter(|it| calc.gifted.contains(&it.key) && changed.iter().any(|c| c.0 == it.key))
            .flat_map(|it| std::iter::once(it.product_id).chain(it.parent.as_ref().map(|p| p.id)))
            .collect();
        gifts.extend(available_gifts(rule, &gifted));
        let delivery = round2(old_delivery.unwrap_or(0.0) - calc.delivery.unwrap_or(0.0));
        if changed.is_empty() && calc.delivery == old_delivery {
            continue;
        }

        let items: Vec<(i64, f64)> = changed
            .iter()
            .map(|&(key, qty)| (key, round2((old_prices[&key] - calc.prices[&key]) * qty)))
            .collect();
        for &(key, _) in &changed {
            calc.applied.entry(key).or_default().push(rule.id);
        }
        let sum = round2(items.iter().map(|(_, s)| s).sum::<f64>() + delivery);
        if let Some(i) = coupon {
            coupons[i] = CouponStatus::Applied;
        }
        applied.push(Applied {
            discount_id: rule.id,
            name: rule.name.clone(),
            coupon: coupon_data.map(|cp| cp.coupon.clone()),
            items,
            delivery,
            sum,
        });
        if rule.last_discount {
            break;
        }
        if rule.last_level_discount {
            skip_level = Some(rule.priority);
        }
    }

    let items = ctx
        .items
        .iter()
        .map(|it| {
            let mut price = calc.price(it.key);
            // Округление — в самом конце: условия и суммы правил видят неокруглённые цены
            if let Some(rounding) = ctx.rounding.as_ref().filter(|_| !it.custom_price) {
                price = rounding.round(it.price_type_id, price);
            }
            let res = ItemResult {
                price,
                discount: round2(it.base_price - price),
                rules: calc.applied.get(&it.key).cloned().unwrap_or_default(),
            };
            (it.key, res)
        })
        .collect();
    DiscountResult {
        items,
        delivery_price: calc.delivery,
        applied,
        coupons: ctx
            .coupons
            .iter()
            .zip(coupons)
            .map(|(cp, st)| (cp.coupon.clone(), st))
            .collect(),
        gifts,
    }
}

/// Подарки правила без уже подаренных им товаров `gifted` (`deleteGiftedProducts`): `one` с
/// подаренным исчезает, у `all` подаренные убираются, пустой исчезает; раздел `one` исчезает,
/// если правило что-то подарило, `all` остаётся.
fn available_gifts(rule: &Rule, gifted: &[i64]) -> Vec<GiftOffer> {
    rule.actions
        .children
        .iter()
        .filter_map(gift_spec)
        .filter_map(|(kind, mut product_ids, section_id)| {
            match (kind, section_id) {
                (GiftType::One, Some(_)) if !gifted.is_empty() => return None,
                (GiftType::One, None) if product_ids.iter().any(|p| gifted.contains(p)) => {
                    return None;
                }
                (GiftType::All, None) => {
                    product_ids.retain(|p| !gifted.contains(p));
                    if product_ids.is_empty() {
                        return None;
                    }
                }
                _ => {}
            }
            Some(GiftOffer {
                rule_id: rule.id,
                kind,
                product_ids,
                section_id,
                gifted: gifted.to_vec(),
            })
        })
        .collect()
}

/// Правила для цены в каталоге (как `GetOptimalPrice`): без условий корзины и заказа
/// (`CondBskt*`, `CondSale*`), без купонов, без накопительных классов и подарков (у Битрикса
/// `EXECUTE_MODULE = sale`).
pub fn product_rules(rules: &[Rule]) -> Vec<&Rule> {
    fn order_bound(node: &Node) -> bool {
        node.class_id.starts_with("CondBskt")
            || node.class_id.starts_with("CondSale")
            || node.class_id == "CondCumulativeGroup"
            || node.children.iter().any(order_bound)
    }
    // Накопительные и подарки — только в корзине
    fn sale_only(node: &Node) -> bool {
        matches!(node.class_id.as_str(), "CumulativeAction" | "GiftCondGroup")
            || node.children.iter().any(sale_only)
    }
    rules
        .iter()
        .filter(|r| !r.use_coupons && !order_bound(&r.conditions) && !sale_only(&r.actions))
        .collect()
}

/// Цена одной единицы товара в каталоге после правил `rules` (обычно [`product_rules`]) для
/// покупателя с группами `group_ids` (гость — без групп: действуют правила без групп), как
/// `CCatalogProduct::GetOptimalPrice`: позиция в 1 шт. одна в корзине, без купонов и данных
/// заказа. `None` — правила цену не меняют.
pub fn catalog_price(
    rules: &[&Rule],
    item: &CtxItem,
    group_ids: &[i64],
    now: DateTime<Utc>,
) -> Option<f64> {
    catalog_price_with(rules, item, None, group_ids, now, None, None)
}

/// [`catalog_price`] для посетителя `user_id` (его видят условия проекта `hook`) и правил
/// округления `rounding`: цена в каталоге — округлённая; если она равна округлённой базовой,
/// правила цену не меняют.
pub fn catalog_price_with(
    rules: &[&Rule],
    item: &CtxItem,
    user_id: Option<i64>,
    group_ids: &[i64],
    now: DateTime<Utc>,
    hook: Hook,
    rounding: Option<&Rounding>,
) -> Option<f64> {
    if rules.is_empty() || item.custom_price {
        return None;
    }
    let ctx = Context {
        items: vec![CtxItem {
            quantity: 1.0,
            ..item.clone()
        }],
        user_id,
        group_ids: group_ids.to_vec(),
        coupons: Vec::new(),
        order: Default::default(),
        now,
        rounding: rounding.cloned(),
        cumulative: HashMap::new(),
    };
    let price = calculate_refs(rules, &ctx, hook)
        .items
        .get(&item.key)?
        .price;
    let base = rounding.map_or(item.base_price, |r| {
        r.round(item.price_type_id, item.base_price)
    });
    (price != base).then_some(price)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::discount::context::{CtxItem, OrderCtx};
    use crate::discount::cumulative::CumulativePeriod;
    use crate::discount::model::{Coupon, CouponKind, Node};

    fn node(v: Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    fn always() -> Node {
        node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []}),
        )
    }

    fn never() -> Node {
        // Сумма корзины больше миллиона
        node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 1_000_000}}
            ]}),
        )
    }

    /// `ActSaleBsktGrp` с фильтром позиций `filter`.
    fn bskt(kind: &str, value: f64, unit: &str, max: f64, filter: Vec<Value>) -> Value {
        json!({
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": kind, "Value": value, "Unit": unit, "Max": max, "All": "AND", "True": "True"},
            "CHILDREN": filter
        })
    }

    fn delivery(kind: &str, value: f64, unit: &str) -> Value {
        json!({"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": kind, "Value": value, "Unit": unit}})
    }

    fn actions(list: Vec<Value>) -> Node {
        node(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": list}))
    }

    /// Фильтр «позиция — товар `id`».
    fn product(id: i64) -> Value {
        json!({"CLASS_ID": "CondBsktFldProduct", "DATA": {"logic": "Equal", "value": id}})
    }

    fn rule(id: i64, acts: Vec<Value>) -> Rule {
        Rule {
            id,
            name: format!("Правило {id}"),
            active: true,
            active_from: None,
            active_to: None,
            priority: 1,
            sort: 100,
            last_discount: false,
            last_level_discount: false,
            use_coupons: false,
            conditions: always(),
            actions: actions(acts),
            group_ids: vec![],
            unsupported: String::new(),
            xml_id: String::new(),
        }
    }

    /// Позиция `key`: товар `key * 10`, цена `price`, количество `qty`.
    fn item(key: i64, price: f64, qty: f64) -> CtxItem {
        CtxItem {
            key,
            product_id: key * 10,
            collection_id: 4,
            name: format!("Товар {key}"),
            quantity: qty,
            base_price: price,
            ..CtxItem::default()
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
    }

    fn ctx(items: Vec<CtxItem>) -> Context {
        Context {
            items,
            user_id: Some(5),
            group_ids: vec![2],
            coupons: vec![],
            order: OrderCtx::default(),
            now: now(),
            rounding: None,
            cumulative: HashMap::new(),
        }
    }

    fn price(r: &DiscountResult, key: i64) -> f64 {
        r.items[&key].price
    }

    fn ids(r: &DiscountResult) -> Vec<i64> {
        r.applied.iter().map(|a| a.discount_id).collect()
    }

    fn coupon(code: &str, discount_id: i64) -> Coupon {
        Coupon {
            id: 1,
            discount_id,
            coupon: code.to_string(),
            active: true,
            active_from: None,
            active_to: None,
            kind: CouponKind::Multi,
            max_use: 0,
            use_count: 0,
            user_id: None,
        }
    }

    fn with_rounding(mut c: Context, t: crate::catalog::rounding::RoundType, p: f64) -> Context {
        use crate::catalog::rounding::{RoundRule, Rounding};
        c.rounding = Some(Rounding::new(vec![RoundRule {
            price_type_id: 1,
            price: 0.0,
            round_type: t,
            precision: p,
        }]));
        for it in &mut c.items {
            it.price_type_id = Some(1);
        }
        c
    }

    #[test]
    fn rounding_applies_to_final_price() {
        use crate::catalog::rounding::RoundType;
        let c = with_rounding(ctx(vec![item(1, 1234.56, 2.0)]), RoundType::Up, 10.0);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        // 1234.56 − 123.46 = 1111.10 → вверх до 10 = 1120
        assert_eq!(
            r.items[&1],
            ItemResult {
                price: 1120.0,
                discount: 114.56,
                rules: vec![1]
            }
        );
        // Сумма правила — только его скидка, без округления
        assert_eq!(r.applied[0].items, vec![(1, 246.92)]);
        assert_eq!(r.applied[0].sum, 246.92);
    }

    #[test]
    fn rounding_skips_custom_price_and_zero() {
        use crate::catalog::rounding::RoundType;
        let mut custom = item(1, 1234.56, 1.0);
        custom.custom_price = true;
        let c = with_rounding(ctx(vec![custom, item(2, 0.0, 1.0)]), RoundType::Up, 10.0);
        let r = calculate(&[], &c, None);
        assert_eq!(price(&r, 1), 1234.56);
        assert_eq!(price(&r, 2), 0.0);
    }

    #[test]
    fn percent_discount_and_extra() {
        let c = ctx(vec![item(1, 1000.0, 2.0)]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(
            r.items[&1],
            ItemResult {
                price: 900.0,
                discount: 100.0,
                rules: vec![1]
            }
        );
        assert_eq!(r.applied.len(), 1);
        assert_eq!(r.applied[0].name, "Правило 1");
        assert_eq!(r.applied[0].coupon, None);
        assert_eq!(r.applied[0].items, vec![(1, 200.0)]);
        assert_eq!(r.applied[0].delivery, 0.0);
        assert_eq!(r.applied[0].sum, 200.0);
        assert_eq!(r.delivery_price, None);

        let r = calculate(
            &[rule(1, vec![bskt("Extra", 12.5, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 1125.0);
        assert_eq!(r.items[&1].discount, -125.0);
        assert_eq!(r.applied[0].items, vec![(1, -250.0)]);

        // Процент округляется до копеек на единицу
        let c = ctx(vec![item(1, 99.99, 3.0)]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 15.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 84.99); // 99.99 − 15.00
        assert_eq!(r.applied[0].items, vec![(1, 45.0)]);

        // Без правил — базовые цены, ничего не применено
        let r = calculate(&[], &c, None);
        assert_eq!(
            r.items[&1],
            ItemResult {
                price: 99.99,
                discount: 0.0,
                rules: vec![]
            }
        );
        assert!(r.applied.is_empty());
    }

    #[test]
    fn cur_each_and_closeout() {
        let c = ctx(vec![item(1, 1000.0, 2.0), item(2, 500.0, 1.0)]);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 150.0, "CurEach", 0.0, vec![])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 850.0);
        assert_eq!(price(&r, 2), 350.0);
        assert_eq!(r.applied[0].items, vec![(1, 300.0), (2, 150.0)]);
        assert_eq!(r.applied[0].sum, 450.0);

        let r = calculate(
            &[rule(1, vec![bskt("Extra", 20.0, "CurEach", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 1020.0);

        // Распродажа: цена ставится, только если ниже текущей
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Closeout", 700.0, "CurEach", 0.0, vec![])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 700.0);
        assert_eq!(price(&r, 2), 500.0);
        assert_eq!(r.items[&2].rules, Vec::<i64>::new());
        assert_eq!(r.applied[0].items, vec![(1, 600.0)]);

        let r = calculate(
            &[rule(
                1,
                vec![bskt("Closeout", 1200.0, "CurEach", 0.0, vec![])],
            )],
            &c,
            None,
        );
        assert!(r.applied.is_empty());
        assert_eq!(price(&r, 1), 1000.0);

        // Распродажа только в валюте (`Closeout` с процентом Битрикс не генерирует)
        let r = calculate(
            &[rule(1, vec![bskt("Closeout", 10.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert!(r.applied.is_empty());
    }

    #[test]
    fn cur_all_distribution_is_exact() {
        let c = ctx(vec![
            item(1, 100.0, 1.0),
            item(2, 100.0, 1.0),
            item(3, 100.01, 1.0),
        ]);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 300.0, "CurAll", 0.0, vec![])],
            )],
            &c,
            None,
        );
        // По процентам 100.00 / 100.00 / 100.01 = 300.01; лишняя копейка снимается с позиции
        // с наименьшим количеством (из равных — с последней)
        assert_eq!(r.applied[0].items, vec![(1, 100.0), (2, 100.0), (3, 100.0)]);
        assert_eq!(r.applied[0].sum, 300.0);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(price(&r, 2), 0.0);
        assert_eq!(price(&r, 3), 0.01);

        // Пропорционально стоимости позиций; остаток — на последней
        let c = ctx(vec![
            item(1, 200.0, 2.0),
            item(2, 100.0, 1.0),
            item(3, 33.33, 1.0),
        ]);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 100.0, "CurAll", 0.0, vec![])],
            )],
            &c,
            None,
        );
        let sums: Vec<f64> = r.applied[0].items.iter().map(|(_, s)| *s).collect();
        // 100 / 533.33: 200 → 37.50 за единицу, 100 → 18.75, остаток 100 − 75 − 18.75 = 6.25
        assert_eq!(sums, vec![75.0, 18.75, 6.25]);
        assert_eq!(r.applied[0].sum, 100.0);
        assert_eq!(price(&r, 3), 27.08);

        // Наценка суммой
        let r = calculate(
            &[rule(1, vec![bskt("Extra", 10.0, "CurAll", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(r.applied[0].sum, -10.0);

        // Остаток берёт позиция с количеством 1, а не последняя (кол-во 3): по процентам
        // 0.25 + 0.25 × 3 = 1.00, копейка — на первую
        let c = ctx(vec![item(1, 3.33, 1.0), item(2, 3.33, 3.0)]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 1.01, "CurAll", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(r.applied[0].items, vec![(1, 0.26), (2, 0.75)]);
        assert_eq!(r.applied[0].sum, 1.01);

        // Все количества по 2, остаток 1 копейка не делится — ближайшая копейка на единицу
        // у позиции-поглотителя (последней из равных): 0.50 + 0.52 = 1.02
        let c = ctx(vec![item(1, 3.33, 2.0), item(2, 3.33, 2.0)]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 1.01, "CurAll", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(r.applied[0].items, vec![(1, 0.5), (2, 0.52)]);
        assert_eq!(r.applied[0].sum, 1.02);

        // Почти вся корзина (20.44, скидка 20.40): по процентам 2.58 / 9.75 / 7.97 / 0.09 =
        // 20.39; копейка не влезает в 0.09 — её берёт следующая позиция, ни одна не пропущена
        let c = ctx(vec![
            item(1, 2.59, 1.0),
            item(2, 9.77, 1.0),
            item(3, 7.99, 1.0),
            item(4, 0.09, 1.0),
        ]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 20.4, "CurAll", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(
            r.applied[0].items,
            vec![(1, 2.58), (2, 9.75), (3, 7.98), (4, 0.09)]
        );
        assert_eq!(r.applied[0].sum, 20.4);
        assert_eq!(price(&r, 4), 0.0);

        // Скидка больше суммы позиций — не применяется (как Битрикс: процент > 100)
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 1000.0, "CurAll", 0.0, vec![])],
            )],
            &c,
            None,
        );
        assert!(r.applied.is_empty());
    }

    /// Округление — как `round($v, 2)` PHP: 0.145 → 0.15, 0.285 → 0.29, −0.565 → −0.57.
    #[test]
    fn percent_rounds_like_php() {
        let c = ctx(vec![
            item(1, 1.45, 1.0),
            item(2, 1.9, 1.0),
            item(3, 5.65, 1.0),
        ]);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 10.0, "Perc", 0.0, vec![product(10)])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 1.3);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 15.0, "Perc", 0.0, vec![product(20)])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 2), 1.61);
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 10.0, "Perc", 0.0, vec![product(30)])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 3), 5.08);
        assert_eq!(r.applied[0].items, vec![(3, 0.57)]);
    }

    #[test]
    fn max_limits_percent() {
        let c = ctx(vec![item(1, 1000.0, 2.0), item(2, 100.0, 1.0)]);
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 50.0, "Perc", 100.0, vec![])])],
            &c,
            None,
        );
        // Не больше 100 на единицу (`LIMIT_VALUE` Битрикса)
        assert_eq!(price(&r, 1), 900.0);
        assert_eq!(price(&r, 2), 50.0);
        assert_eq!(r.applied[0].items, vec![(1, 200.0), (2, 50.0)]);

        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 50.0, "Perc", 1000.0, vec![])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 500.0);

        // У наценки ограничения нет
        let r = calculate(
            &[rule(1, vec![bskt("Extra", 50.0, "Perc", 100.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 1500.0);
    }

    #[test]
    fn delivery_actions() {
        let mut c = ctx(vec![item(1, 1000.0, 1.0)]);
        c.order.delivery_price = Some(500.0);
        let run = |act: Value, c: &Context| calculate(&[rule(1, vec![act])], c, None);

        let r = run(delivery("Discount", 10.0, "Perc"), &c);
        assert_eq!(r.delivery_price, Some(450.0));
        assert_eq!(r.applied[0].delivery, 50.0);
        assert_eq!(r.applied[0].sum, 50.0);
        assert!(r.applied[0].items.is_empty());
        assert_eq!(price(&r, 1), 1000.0);

        let r = run(delivery("Discount", 100.0, "Cur"), &c);
        assert_eq!(r.delivery_price, Some(400.0));
        let r = run(delivery("Extra", 50.0, "Cur"), &c);
        assert_eq!(r.delivery_price, Some(550.0));
        assert_eq!(r.applied[0].delivery, -50.0);
        let r = run(delivery("Extra", 10.0, "Perc"), &c);
        assert_eq!(r.delivery_price, Some(550.0));

        // «Скидку не более»: до нуля
        let r = run(delivery("DiscountZero", 1000.0, "Cur"), &c);
        assert_eq!(r.delivery_price, Some(0.0));
        assert_eq!(r.applied[0].delivery, 500.0);
        let r = run(delivery("DiscountZero", 100.0, "Perc"), &c);
        assert_eq!(r.delivery_price, Some(0.0));

        // Обычная скидка больше цены — не применяется
        let r = run(delivery("Discount", 1000.0, "Cur"), &c);
        assert_eq!(r.delivery_price, Some(500.0));
        assert!(r.applied.is_empty());

        // Бесплатную доставку не скидывают, а наценка на неё — есть
        c.order.delivery_price = Some(0.0);
        assert!(
            run(delivery("Discount", 10.0, "Cur"), &c)
                .applied
                .is_empty()
        );
        assert_eq!(
            run(delivery("Extra", 10.0, "Cur"), &c).delivery_price,
            Some(10.0)
        );

        // Без доставки в контексте — ничего
        c.order.delivery_price = None;
        let r = run(delivery("Discount", 10.0, "Cur"), &c);
        assert_eq!(r.delivery_price, None);
        assert!(r.applied.is_empty());

        // Позиции и доставка в одном правиле
        c.order.delivery_price = Some(300.0);
        let r = calculate(
            &[rule(
                1,
                vec![
                    bskt("Discount", 10.0, "Perc", 0.0, vec![]),
                    delivery("Discount", 100.0, "Cur"),
                ],
            )],
            &c,
            None,
        );
        assert_eq!(r.applied[0].sum, 200.0);
        assert_eq!(r.delivery_price, Some(200.0));
    }

    #[test]
    fn priority_and_sort_order() {
        let mut rules = vec![
            rule(5, vec![bskt("Discount", 10.0, "CurEach", 0.0, vec![])]),
            rule(3, vec![bskt("Discount", 10.0, "CurEach", 0.0, vec![])]),
            rule(9, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]),
            rule(2, vec![bskt("Discount", 10.0, "CurEach", 0.0, vec![])]),
        ];
        rules[0].sort = 200;
        rules[2].priority = 2;
        rules[2].sort = 500;
        let c = ctx(vec![item(1, 1000.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![9, 2, 3, 5]);
        assert_eq!(r.items[&1].rules, vec![9, 2, 3, 5]);
        // Процент — первым, от базовой: 1000 − 100 − 10 − 10 − 10
        assert_eq!(price(&r, 1), 870.0);
    }

    #[test]
    fn last_discount_stops_all() {
        let mut rules = vec![
            rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]),
            rule(2, vec![bskt("Discount", 10.0, "CurEach", 0.0, vec![])]),
            rule(3, vec![bskt("Discount", 5.0, "CurEach", 0.0, vec![])]),
        ];
        rules[0].priority = 3;
        rules[0].last_discount = true;
        rules[1].priority = 3;
        rules[1].sort = 200;
        let c = ctx(vec![item(1, 1000.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![1]);
        assert_eq!(price(&r, 1), 900.0);

        // Не сработавшее правило ничего не останавливает
        rules[0].conditions = never();
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![2, 3]);

        // Сработало по условиям, но ничего не изменило (фильтр позиций пуст) — тоже
        rules[0].conditions = always();
        rules[0].actions = actions(vec![bskt(
            "Discount",
            10.0,
            "Perc",
            0.0,
            vec![product(999)],
        )]);
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![2, 3]);
    }

    /// `LAST_LEVEL_DISCOUNT` Битрикса — «прекратить применение скидок на текущем уровне
    /// приоритетов»: правила того же приоритета пропускаются, меньшего — применяются.
    #[test]
    fn last_level_discount_stops_current_level() {
        let mut rules = vec![
            rule(1, vec![bskt("Discount", 10.0, "CurEach", 0.0, vec![])]),
            rule(2, vec![bskt("Discount", 20.0, "CurEach", 0.0, vec![])]),
            rule(3, vec![bskt("Discount", 30.0, "CurEach", 0.0, vec![])]),
            rule(4, vec![bskt("Discount", 40.0, "CurEach", 0.0, vec![])]),
        ];
        rules[0].priority = 5;
        rules[0].last_level_discount = true;
        rules[1].priority = 5;
        rules[2].priority = 1;
        rules[3].priority = 1;
        let c = ctx(vec![item(1, 1000.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![1, 3, 4]);
        assert_eq!(price(&r, 1), 920.0);
    }

    #[test]
    fn coupon_rule_needs_active_coupon() {
        let mut ok = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        ok.use_coupons = true;
        let mut failing = rule(2, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        failing.use_coupons = true;
        failing.conditions = never();
        let rules = vec![ok, failing];

        // Без купона правило не действует
        let mut c = ctx(vec![item(1, 1000.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert!(r.applied.is_empty());
        assert!(r.coupons.is_empty());

        let past = now() - Duration::days(1);
        let future = now() + Duration::days(1);
        let mut expired = coupon("EXP", 1);
        expired.active_to = Some(past);
        let mut not_yet = coupon("SOON", 1);
        not_yet.active_from = Some(future);
        let mut used = coupon("USED", 1);
        used.max_use = 2;
        used.use_count = 2;
        let mut foreign = coupon("ALIEN", 1);
        foreign.user_id = Some(99);
        let mut off = coupon("OFF", 1);
        off.active = false;
        let mut mine = coupon("MINE", 1);
        mine.user_id = Some(5);
        c.coupons = vec![
            expired,
            not_yet,
            used,
            foreign,
            off,
            coupon("NA", 2),
            mine,
            coupon("SECOND", 1),
        ];
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![1]);
        assert_eq!(r.applied[0].coupon.as_deref(), Some("MINE"));
        assert_eq!(price(&r, 1), 900.0);
        assert_eq!(
            r.coupons,
            vec![
                ("EXP".to_string(), CouponStatus::Expired),
                ("SOON".to_string(), CouponStatus::Inactive),
                ("USED".to_string(), CouponStatus::Used),
                ("ALIEN".to_string(), CouponStatus::Foreign),
                ("OFF".to_string(), CouponStatus::Inactive),
                ("NA".to_string(), CouponStatus::NotApplied),
                ("MINE".to_string(), CouponStatus::Applied),
                // Правило уже применено по другому купону
                ("SECOND".to_string(), CouponStatus::NotApplied),
            ]
        );

        // Только негодные купоны — правило не действует
        c.coupons
            .retain(|cp| cp.coupon == "EXP" || cp.coupon == "USED");
        assert!(calculate(&rules, &c, None).applied.is_empty());

        // Купон правила, которое неактивно или вне дат, — не активен / истёк
        let mut rules = rules;
        rules[0].active = false;
        rules[1].active_to = Some(past);
        c.coupons = vec![coupon("A", 1), coupon("B", 2), coupon("GONE", 77)];
        let r = calculate(&rules, &c, None);
        assert_eq!(
            r.coupons,
            vec![
                ("A".to_string(), CouponStatus::Inactive),
                ("B".to_string(), CouponStatus::Expired),
                ("GONE".to_string(), CouponStatus::Inactive),
            ]
        );
    }

    /// Купон «на одну позицию» (`TYPE_BASKET_ROW`): скидка остаётся на самой дорогой позиции.
    #[test]
    fn basket_coupon_applies_to_one_row() {
        let mut r1 = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        r1.use_coupons = true;
        let mut c = ctx(vec![
            item(1, 100.0, 1.0),
            item(2, 300.0, 1.0),
            item(3, 300.0, 2.0),
        ]);
        let mut cp = coupon("ROW", 1);
        cp.kind = CouponKind::Basket;
        c.coupons = vec![cp];
        let r = calculate(&[r1], &c, None);
        assert_eq!(r.applied[0].items, vec![(2, 30.0)]);
        assert_eq!(price(&r, 1), 100.0);
        assert_eq!(price(&r, 2), 270.0);
        assert_eq!(price(&r, 3), 300.0);
        assert!(r.items[&3].rules.is_empty());
        assert_eq!(r.coupons, vec![("ROW".to_string(), CouponStatus::Applied)]);
    }

    #[test]
    fn groups_and_dates_filter() {
        let c = ctx(vec![item(1, 1000.0, 1.0)]);
        let base = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        let applies = |r: Rule, c: &Context| !calculate(&[r], c, None).applied.is_empty();

        assert!(applies(base.clone(), &c));
        let mut r = base.clone();
        r.group_ids = vec![16, 17];
        assert!(!applies(r.clone(), &c));
        let mut c16 = c.clone();
        c16.group_ids = vec![2, 17];
        assert!(applies(r, &c16));

        let mut r = base.clone();
        r.active = false;
        assert!(!applies(r, &c));
        let mut r = base.clone();
        r.active_from = Some(now() + Duration::hours(1));
        assert!(!applies(r, &c));
        let mut r = base.clone();
        r.active_to = Some(now() - Duration::hours(1));
        assert!(!applies(r, &c));
        let mut r = base.clone();
        r.active_from = Some(now() - Duration::hours(1));
        r.active_to = Some(now() + Duration::hours(1));
        assert!(applies(r, &c));
        let mut r = base;
        r.unsupported = "CondBsktCustom".to_string();
        assert!(!applies(r, &c));
    }

    #[test]
    fn custom_price_untouched() {
        let mut fixed = item(1, 1500.0, 1.0);
        fixed.custom_price = true;
        let c = ctx(vec![fixed, item(2, 1000.0, 1.0)]);
        // Условие видит и позицию со своей ценой: сумма 2500 ≥ 2000
        let mut r1 = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        r1.conditions = node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondBsktAmtGroup", "DATA": {"logic": "EqGr", "Value": 2000, "All": "AND"}, "CHILDREN": []}
            ]}),
        );
        let r = calculate(&[r1], &c, None);
        assert_eq!(
            r.items[&1],
            ItemResult {
                price: 1500.0,
                discount: 0.0,
                rules: vec![]
            }
        );
        assert_eq!(price(&r, 2), 900.0);
        assert_eq!(r.applied[0].items, vec![(2, 100.0)]);
    }

    #[test]
    fn price_never_negative() {
        let c = ctx(vec![item(1, 1000.0, 1.0), item(2, 100.0, 1.0)]);
        // Скидка больше цены позиции — к ней не применяется (как Битрикс), к прочим — да
        let r = calculate(
            &[rule(
                1,
                vec![bskt("Discount", 500.0, "CurEach", 0.0, vec![])],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 500.0);
        assert_eq!(price(&r, 2), 100.0);
        assert!(r.items[&2].rules.is_empty());

        let r = calculate(
            &[rule(1, vec![bskt("Discount", 150.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert!(r.applied.is_empty());
        assert!(r.items.values().all(|it| it.price >= 0.0));

        // Ровно до нуля — можно
        let r = calculate(
            &[rule(1, vec![bskt("Discount", 100.0, "Perc", 0.0, vec![])])],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(r.items[&1].discount, 1000.0);
    }

    #[test]
    fn sequential_application_uses_current_price() {
        let c = ctx(vec![item(1, 1000.0, 1.0)]);
        let mut second = rule(2, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        second.sort = 200;
        let rules = vec![
            rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]),
            second,
        ];
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 810.0);
        assert_eq!(r.applied[1].items, vec![(1, 90.0)]);
        assert_eq!(r.items[&1].discount, 190.0);

        // Фильтр действия видит текущую цену: «цена < 950» — уже 900
        let mut third = rule(
            3,
            vec![bskt(
                "Discount",
                50.0,
                "CurEach",
                0.0,
                vec![
                    json!({"CLASS_ID": "CondBsktFldPrice", "DATA": {"logic": "Less", "value": 950}}),
                ],
            )],
        );
        third.sort = 300;
        let mut rules = vec![rules[0].clone(), third];
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 850.0);

        // Условие «к позиции применена скидка» видит предыдущие правила
        rules[1].actions = actions(vec![bskt(
            "Discount",
            50.0,
            "CurEach",
            0.0,
            vec![json!({"CLASS_ID": "CondBsktAppliedDiscount", "DATA": {"value": "N"}})],
        )]);
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![1]);

        // Второе действие правила — от цены после первого
        let r = calculate(
            &[rule(
                1,
                vec![
                    bskt("Discount", 10.0, "Perc", 0.0, vec![]),
                    bskt("Discount", 10.0, "Perc", 0.0, vec![]),
                ],
            )],
            &c,
            None,
        );
        assert_eq!(price(&r, 1), 810.0);
        assert_eq!(r.applied[0].items, vec![(1, 190.0)]);
        assert_eq!(r.items[&1].rules, vec![1]);
    }

    /// Правила трансопта №24–27 (`b_sale_discount`): наценка или скидка на объявления
    /// Transopt (свойство 55 «Объявление Transopt» = вариант 32 «Y») по группе покупателя.
    fn transopt_rule(id: i64, name: &str, group: i64, kind: &str, value: f64, last: bool) -> Rule {
        Rule {
            id,
            name: name.to_string(),
            active: true,
            active_from: None,
            active_to: None,
            priority: 1,
            sort: 100,
            last_discount: last,
            last_level_discount: false,
            use_coupons: false,
            conditions: always(),
            actions: node(json!({
                "CLASS_ID": "CondGroup",
                "DATA": {"All": "AND"},
                "CHILDREN": [{
                    "CLASS_ID": "ActSaleBsktGrp",
                    "DATA": {"Type": kind, "Value": value, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
                    "CHILDREN": [{"CLASS_ID": "CondIBProp:4:55", "DATA": {"logic": "Equal", "value": 32}}]
                }]
            })),
            group_ids: vec![group],
            unsupported: String::new(),
            xml_id: String::new(),
        }
    }

    fn fields(v: Value) -> Map<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn transopt_rules() {
        let rules = vec![
            transopt_rule(24, "+10%", 16, "Extra", 10.0, false),
            transopt_rule(25, "-10%", 17, "Discount", 10.0, true),
            transopt_rule(26, "+5%", 18, "Extra", 5.0, true),
            transopt_rule(27, "-5%", 19, "Discount", 5.0, true),
        ];
        let mut transopt = item(1, 1000.0, 1.0);
        transopt.field_ids = HashMap::from([(55, "transopt".to_string())]);
        transopt.fields = fields(json!({"transopt": 32}));
        let mut other = item(2, 1000.0, 1.0);
        other.field_ids = HashMap::from([(55, "transopt".to_string())]);
        let mut c = ctx(vec![transopt, other]);

        c.group_ids = vec![2, 16];
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 1100.0);
        assert_eq!(price(&r, 2), 1000.0);
        assert_eq!(ids(&r), vec![24]);
        assert_eq!(r.applied[0].name, "+10%");

        c.group_ids = vec![2, 19];
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 950.0);
        assert_eq!(price(&r, 2), 1000.0);
        assert_eq!(ids(&r), vec![27]);

        c.group_ids = vec![2, 17];
        assert_eq!(price(&calculate(&rules, &c, None), 1), 900.0);

        c.group_ids = vec![2];
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 1000.0);
        assert!(r.applied.is_empty());

        // В двух группах: «-10%» (№25) — последнее, «-5%» уже не применяется
        c.group_ids = vec![17, 19];
        let r = calculate(&rules, &c, None);
        assert_eq!(ids(&r), vec![25]);
        assert_eq!(price(&r, 1), 900.0);
    }

    #[test]
    fn product_rules_excludes_basket_conditions() {
        let with = |id: i64, cond: Value| {
            let mut r = rule(id, vec![]);
            r.conditions = node(cond);
            r
        };
        let grp = |children: Value| json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": children});
        let mut coupon_rule = rule(5, vec![]);
        coupon_rule.use_coupons = true;
        let rules = vec![
            rule(1, vec![]),
            with(
                2,
                grp(
                    json!([{"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 1}}]),
                ),
            ),
            with(
                3,
                grp(json!([grp(
                    json!([{"CLASS_ID": "CondBsktCntGroup", "DATA": {"logic": "Great", "Value": 1, "All": "AND"}, "CHILDREN": []}])
                )])),
            ),
            with(
                4,
                grp(
                    json!([{"CLASS_ID": "CondSaleCmnDayOfWeek", "DATA": {"logic": "Equal", "value": [1]}}]),
                ),
            ),
            coupon_rule,
            with(
                6,
                grp(json!([{"CLASS_ID": "CondIBSection", "DATA": {"logic": "Equal", "value": 3}}])),
            ),
        ];
        let ids: Vec<i64> = product_rules(&rules).iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 6]);
    }

    #[test]
    fn catalog_price_one_unit_product_rules() {
        // Позиция товара 10: правило −10 % только на него, −15 руб. на все (CurAll → на 1 шт.)
        let mut only_10 = rule(
            1,
            vec![bskt("Discount", 10.0, "Perc", 0.0, vec![product(10)])],
        );
        only_10.priority = 2;
        let all = rule(2, vec![bskt("Discount", 15.0, "CurAll", 0.0, vec![])]);
        let mut vip = rule(3, vec![bskt("Discount", 50.0, "Perc", 0.0, vec![])]);
        vip.group_ids = vec![7];
        let rules = [only_10, all, vip];
        let refs: Vec<&Rule> = rules.iter().collect();
        let it = item(1, 1000.0, 5.0);
        // Количество позиции не важно — считается одна единица
        assert_eq!(catalog_price(&refs, &it, &[], now()), Some(885.0));
        // Группа покупателя включает правило по группам
        assert_eq!(catalog_price(&refs, &it, &[7], now()), Some(442.5));
        // Другой товар: только общее правило
        assert_eq!(
            catalog_price(&refs, &item(2, 1000.0, 1.0), &[], now()),
            Some(985.0)
        );
        // Нет правил или цена не изменилась — None
        assert_eq!(catalog_price(&[], &it, &[], now()), None);
        let zero = rule(4, vec![bskt("Discount", 0.0, "Perc", 0.0, vec![])]);
        assert_eq!(catalog_price(&[&zero], &it, &[], now()), None);
        // Наценка тоже меняет цену
        let extra = rule(5, vec![bskt("Extra", 10.0, "Perc", 0.0, vec![])]);
        assert_eq!(catalog_price(&[&extra], &it, &[], now()), Some(1100.0));
    }

    struct UserHook;

    impl DiscountConditionHook for UserHook {
        fn class_ids(&self) -> &[&'static str] {
            &["CondProjUser"]
        }

        fn check(
            &self,
            _: &str,
            data: &Map<String, Value>,
            ctx: &Context,
            _: Option<&CtxItem>,
        ) -> Option<bool> {
            Some(ctx.user_id == data["user"].as_i64())
        }
    }

    #[test]
    fn catalog_price_hook_sees_user() {
        let mut r = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        r.conditions = node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondProjUser", "DATA": {"user": 5}}
            ]}),
        );
        let it = item(1, 1000.0, 1.0);
        let hook: Hook = Some(&UserHook);
        assert_eq!(
            catalog_price_with(&[&r], &it, Some(5), &[], now(), hook, None),
            Some(900.0)
        );
        assert_eq!(
            catalog_price_with(&[&r], &it, Some(6), &[], now(), hook, None),
            None
        );
        assert_eq!(
            catalog_price_with(&[&r], &it, None, &[], now(), hook, None),
            None
        );
    }

    /// Условия «накоплено `logic` `value`» с узлом периода `period`.
    fn cumulative_cond(logic: &str, value: f64, period: Option<Value>) -> Node {
        let children: Vec<Value> = period.into_iter().collect();
        node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": logic, "Value": value}, "CHILDREN": children}
            ]}),
        )
    }

    /// `CumulativeAction` за всё время с порогами `ranges` и фильтром позиций `filter`.
    fn cumulative_act(ranges: Value, more_profitable: &str, filter: Vec<Value>) -> Value {
        json!({
            "CLASS_ID": "CumulativeAction",
            "DATA": {
                "ranges": ranges,
                "type_sum_period": "all_time",
                "sum_period_data": {
                    "discount_sum_order_start": "", "discount_sum_order_end": "",
                    "discount_sum_period_value": "", "discount_sum_period_type": ""
                },
                "apply_if_more_profitable": more_profitable,
                "All": "AND",
                "True": "True"
            },
            "CHILDREN": filter
        })
    }

    fn with_sums(mut c: Context, sums: &[(CumulativePeriod, f64)]) -> Context {
        c.cumulative = sums.iter().cloned().collect();
        c
    }

    #[test]
    fn cumulative_condition_logic() {
        let c = with_sums(
            ctx(vec![item(1, 1000.0, 1.0)]),
            &[(CumulativePeriod::AllTime, 5000.0)],
        );
        let run = |c: &Context, logic: &str, value: f64| {
            let mut r = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
            r.conditions = cumulative_cond(logic, value, None);
            !ids(&calculate(&[r], c, None)).is_empty()
        };
        assert!(run(&c, "Great", 4000.0));
        assert!(!run(&c, "Less", 4000.0));
        assert!(run(&c, "EqGr", 5000.0));
        assert!(run(&c, "Equal", 5000.0));
        assert!(!run(&c, "Not", 5000.0));
        // Гость: суммы нет — 0
        let mut guest = ctx(vec![item(1, 1000.0, 1.0)]);
        guest.user_id = None;
        assert!(run(&guest, "Less", 1000.0));
        assert!(!run(&guest, "EqGr", 1000.0));
    }

    #[test]
    fn cumulative_condition_uses_its_period() {
        let c = with_sums(
            ctx(vec![item(1, 1000.0, 1.0)]),
            &[
                (CumulativePeriod::AllTime, 9000.0),
                (
                    CumulativePeriod::Relative {
                        value: 1,
                        unit: 'M',
                    },
                    100.0,
                ),
            ],
        );
        let mut r = rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        r.conditions = cumulative_cond(
            "Great",
            1000.0,
            Some(
                json!({"CLASS_ID": "PeriodRelative", "DATA": {"Value": 1, "TypeRelativePeriod": "M"}}),
            ),
        );
        let res = calculate(std::slice::from_ref(&r), &c, None);
        assert!(res.applied.is_empty());
        assert_eq!(price(&res, 1), 1000.0);
        // Без узла периода — за всё время
        r.conditions = cumulative_cond("Great", 1000.0, None);
        assert_eq!(price(&calculate(&[r], &c, None), 1), 900.0);
    }

    #[test]
    fn cumulative_action_picks_highest_reached_range() {
        let ranges = json!([
            {"sum": 20000, "value": 10, "type": "P"},
            {"sum": 5000, "value": 7, "type": "P"},
            {"sum": 1000, "value": 3, "type": "P"}
        ]);
        let rules = [rule(1, vec![cumulative_act(ranges, "N", vec![])])];
        let at = |sum: f64| {
            with_sums(
                ctx(vec![item(1, 1000.0, 2.0)]),
                &[(CumulativePeriod::AllTime, sum)],
            )
        };
        let r = calculate(&rules, &at(6000.0), None);
        assert_eq!(price(&r, 1), 930.0);
        assert_eq!(ids(&r), vec![1]);
        assert_eq!(r.applied[0].items, vec![(1, 140.0)]);
        assert_eq!(r.applied[0].sum, 140.0);
        assert_eq!(r.items[&1].rules, vec![1]);

        let r = calculate(&rules, &at(999.0), None);
        assert_eq!(price(&r, 1), 1000.0);
        assert!(r.applied.is_empty());

        let fixed = [rule(
            1,
            vec![cumulative_act(
                json!([{"sum": 0, "value": 50, "type": "F"}]),
                "N",
                vec![],
            )],
        )];
        assert_eq!(price(&calculate(&fixed, &at(0.0), None), 1), 950.0);
        // Скидка больше цены — к позиции не применяется
        let over = [rule(
            1,
            vec![cumulative_act(
                json!([{"sum": 0, "value": 1500, "type": "F"}]),
                "N",
                vec![],
            )],
        )];
        assert!(calculate(&over, &at(0.0), None).applied.is_empty());
    }

    #[test]
    fn cumulative_action_child_filter() {
        let c = with_sums(
            ctx(vec![item(1, 1000.0, 1.0), item(2, 1000.0, 1.0)]),
            &[(CumulativePeriod::AllTime, 1000.0)],
        );
        let first = rule(
            1,
            vec![bskt("Discount", 5.0, "Perc", 0.0, vec![product(10)])],
        );
        let mut second = rule(
            2,
            vec![cumulative_act(
                json!([{"sum": 1000, "value": 3, "type": "P"}]),
                "N",
                vec![
                    json!({"CLASS_ID": "CondBsktAppliedDiscount", "DATA": {"logic": "Equal", "value": "N"}}),
                ],
            )],
        );
        second.sort = 200;
        let r = calculate(&[first, second], &c, None);
        assert_eq!(price(&r, 1), 950.0);
        assert_eq!(price(&r, 2), 970.0);
        assert_eq!(r.applied[1].items, vec![(2, 30.0)]);
    }

    #[test]
    fn cumulative_more_profitable_keeps_better_discount() {
        let c = with_sums(
            ctx(vec![item(1, 1000.0, 2.0)]),
            &[(CumulativePeriod::AllTime, 1000.0)],
        );
        let run = |percent: f64| {
            let first = rule(1, vec![bskt("Discount", percent, "Perc", 0.0, vec![])]);
            let mut second = rule(
                2,
                vec![cumulative_act(
                    json!([{"sum": 1000, "value": 10, "type": "P"}]),
                    "Y",
                    vec![],
                )],
            );
            second.sort = 200;
            calculate(&[first, second], &c, None)
        };
        // Скидка 20 % выгоднее — позиция не меняется, правила нет в записях
        let r = run(20.0);
        assert_eq!(price(&r, 1), 800.0);
        assert_eq!(ids(&r), vec![1]);
        assert_eq!(r.items[&1].rules, vec![1]);
        // 5 % (950) — накопительная от базовой: 900, в записи — изменение на 50 за единицу
        let r = run(5.0);
        assert_eq!(price(&r, 1), 900.0);
        assert_eq!(r.items[&1].discount, 100.0);
        assert_eq!(ids(&r), vec![1, 2]);
        assert_eq!(r.applied[1].items, vec![(1, 100.0)]);
        assert_eq!(r.applied[1].sum, 100.0);
    }

    #[test]
    fn cumulative_rules_excluded_from_catalog() {
        let mut by_condition = rule(2, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]);
        by_condition.conditions = node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                    {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 0}, "CHILDREN": []}
                ]}
            ]}),
        );
        let rules = vec![
            rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]),
            by_condition,
            rule(
                3,
                vec![cumulative_act(
                    json!([{"sum": 0, "value": 10, "type": "P"}]),
                    "N",
                    vec![],
                )],
            ),
        ];
        let ids: Vec<i64> = product_rules(&rules).iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1]);
    }

    /// Действие «Подарок» (`GiftCondGroup` с одним `Gifter*`).
    fn gift(class: &str, kind: &str, value: Value) -> Value {
        json!({"CLASS_ID": "GiftCondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
            {"CLASS_ID": class, "DATA": {"Type": kind, "Value": value}}
        ]})
    }

    /// Позиция `key` с товаром `product`.
    fn line(key: i64, product: i64, price: f64, qty: f64) -> CtxItem {
        CtxItem {
            product_id: product,
            ..item(key, price, qty)
        }
    }

    #[test]
    fn gift_one_makes_cheapest_free() {
        let rules = [rule(
            1,
            vec![gift("GifterCondIBElement", "one", json!([10, 11]))],
        )];
        let c = ctx(vec![line(1, 10, 300.0, 1.0), line(2, 11, 200.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 2), 0.0);
        assert_eq!(r.items[&2].discount, 200.0);
        assert_eq!(r.items[&2].rules, vec![1]);
        assert_eq!(price(&r, 1), 300.0);
        assert_eq!(ids(&r), vec![1]);
        assert_eq!(r.applied[0].items, vec![(2, 200.0)]);
        assert_eq!(r.applied[0].sum, 200.0);
        assert!(r.gifts.is_empty(), "{:?}", r.gifts);
    }

    #[test]
    fn gift_all_makes_every_match_free() {
        let rules = [rule(
            1,
            vec![gift("GifterCondIBElement", "all", json!([10, 11]))],
        )];
        let c = ctx(vec![line(1, 10, 300.0, 1.0), line(2, 11, 200.0, 1.0)]);
        let r = calculate(&rules, &c, None);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(price(&r, 2), 0.0);
        assert_eq!(r.applied[0].sum, 500.0);
        assert!(r.gifts.is_empty(), "{:?}", r.gifts);

        let r = calculate(&rules, &ctx(vec![line(1, 10, 300.0, 1.0)]), None);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(
            r.gifts,
            vec![GiftOffer {
                rule_id: 1,
                kind: GiftType::All,
                product_ids: vec![11],
                section_id: None,
                gifted: vec![10]
            }]
        );
    }

    #[test]
    fn gift_requires_quantity_one() {
        let rules = [rule(
            1,
            vec![gift("GifterCondIBElement", "one", json!([10, 11]))],
        )];
        let r = calculate(&rules, &ctx(vec![line(1, 10, 300.0, 2.0)]), None);
        assert_eq!(price(&r, 1), 300.0);
        assert!(r.applied.is_empty());
        assert_eq!(
            r.gifts,
            vec![GiftOffer {
                rule_id: 1,
                kind: GiftType::One,
                product_ids: vec![10, 11],
                section_id: None,
                gifted: vec![]
            }]
        );
    }

    #[test]
    fn gift_by_parent_of_offer() {
        use crate::discount::context::CtxParent;
        let rules = [rule(
            1,
            vec![gift("GifterCondIBElement", "one", json!([7]))],
        )];
        let mut offer = line(1, 70, 500.0, 1.0);
        offer.parent = Some(CtxParent {
            id: 7,
            ..CtxParent::default()
        });
        let r = calculate(&rules, &ctx(vec![offer]), None);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(r.items[&1].discount, 500.0);
        assert!(r.gifts.is_empty(), "{:?}", r.gifts);
    }

    #[test]
    fn gift_by_section() {
        let rules = [rule(1, vec![gift("GifterCondIBSection", "one", json!(5))])];
        let mut inside = line(1, 10, 300.0, 1.0);
        inside.section_ids = vec![3, 5];
        let mut outside = line(2, 11, 100.0, 1.0);
        outside.section_ids = vec![3];
        let r = calculate(&rules, &ctx(vec![inside.clone(), outside]), None);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(price(&r, 2), 100.0);
        assert!(r.gifts.is_empty(), "{:?}", r.gifts);

        // Ничего не подарено (2 шт.) — «один из» раздела доступен, подаренного нет
        let mut two = line(1, 10, 300.0, 2.0);
        two.section_ids = vec![5];
        let r = calculate(&rules, &ctx(vec![two]), None);
        assert_eq!(price(&r, 1), 300.0);
        assert_eq!(
            r.gifts,
            vec![GiftOffer {
                rule_id: 1,
                kind: GiftType::One,
                product_ids: vec![],
                section_id: Some(5),
                gifted: vec![]
            }]
        );
    }

    #[test]
    fn gift_section_all_reports_gifted() {
        use crate::discount::context::CtxParent;
        // «Все» из раздела остаётся; подаренное (с родителем предложения) вычтет вызывающий
        let rules = [rule(1, vec![gift("GifterCondIBSection", "all", json!(5))])];
        let mut offer = line(1, 70, 300.0, 1.0);
        offer.section_ids = vec![5];
        offer.parent = Some(CtxParent {
            id: 7,
            ..CtxParent::default()
        });
        let mut paid = line(2, 11, 100.0, 2.0);
        paid.section_ids = vec![5];
        let r = calculate(&rules, &ctx(vec![offer, paid]), None);
        assert_eq!(price(&r, 1), 0.0);
        assert_eq!(price(&r, 2), 100.0);
        assert_eq!(
            r.gifts,
            vec![GiftOffer {
                rule_id: 1,
                kind: GiftType::All,
                product_ids: vec![],
                section_id: Some(5),
                gifted: vec![70, 7]
            }]
        );
    }

    #[test]
    fn gift_offered_only_when_conditions_pass() {
        let mut r1 = rule(1, vec![gift("GifterCondIBElement", "one", json!([99]))]);
        r1.conditions = node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondBsktAmtGroup", "DATA": {"logic": "EqGr", "Value": 5000, "All": "AND"}, "CHILDREN": []}
            ]}),
        );
        let rules = [r1];
        let r = calculate(&rules, &ctx(vec![line(1, 10, 3000.0, 1.0)]), None);
        assert!(r.gifts.is_empty(), "{:?}", r.gifts);
        let r = calculate(&rules, &ctx(vec![line(1, 10, 6000.0, 1.0)]), None);
        assert_eq!(
            r.gifts,
            vec![GiftOffer {
                rule_id: 1,
                kind: GiftType::One,
                product_ids: vec![99],
                section_id: None,
                gifted: vec![]
            }]
        );
        assert_eq!(price(&r, 1), 6000.0);
        assert!(r.applied.is_empty());
    }

    #[test]
    fn gift_skips_custom_price_and_zero() {
        let rules = [rule(
            1,
            vec![gift("GifterCondIBElement", "all", json!([10, 11]))],
        )];
        let mut custom = line(1, 10, 300.0, 1.0);
        custom.custom_price = true;
        let zero = line(2, 11, 0.0, 1.0);
        let r = calculate(&rules, &ctx(vec![custom, zero]), None);
        assert_eq!(price(&r, 1), 300.0);
        assert_eq!(price(&r, 2), 0.0);
        assert!(r.applied.is_empty());
        assert!(r.items[&2].rules.is_empty());
        assert_eq!(r.gifts.len(), 1);
        assert_eq!(r.gifts[0].product_ids, vec![10, 11]);
    }

    #[test]
    fn gift_rules_excluded_from_catalog() {
        let rules = vec![
            rule(1, vec![bskt("Discount", 10.0, "Perc", 0.0, vec![])]),
            rule(2, vec![gift("GifterCondIBElement", "one", json!([10]))]),
        ];
        let ids: Vec<i64> = product_rules(&rules).iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1]);
    }
}
