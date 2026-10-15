//! Разбор CommerceML 2.x / 3.1: один XML-файл превращается в типизированные структуры.
//! Без БД и без DOM: потоковый разбор `quick_xml::Reader`, имена элементов — по локальной части.

use quick_xml::Reader;
use quick_xml::encoding::Decoder;
use quick_xml::events::{BytesStart, Event};

/// Ошибка разбора; текст идёт в ответ 1С (`failure`).
#[derive(Debug, thiserror::Error)]
pub enum CmlError {
    #[error("Файл не является CommerceML (корневой элемент должен быть «КоммерческаяИнформация»)")]
    NotCml,
    #[error("Некорректный XML: {0}")]
    Xml(String),
    #[error("Некорректное число «{0}»")]
    Number(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub classifier: Option<Classifier>,
    pub catalog: Option<Catalog>,
    pub offers: Option<OfferPackage>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Classifier {
    pub id: String,
    pub name: String,
    pub only_changes: bool,
    pub groups: Vec<Group>,
    pub properties: Vec<Property>,
    pub units: Vec<Unit>,
    pub stores: Vec<Store>,
    pub price_types: Vec<PriceType>,
}

/// Дерево групп развёрнуто в список в порядке пакета (родитель раньше потомков).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Group {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PropKind {
    Directory,
    #[default]
    String,
    Number,
    Date,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Property {
    pub id: String,
    pub name: String,
    pub deleted: bool,
    pub kind: PropKind,
    pub multiple: bool,
    /// (ид значения, значение)
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Store {
    pub id: String,
    pub name: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PriceType {
    pub id: String,
    pub name: String,
    pub currency: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub id: String,
    pub classifier_id: String,
    pub name: String,
    pub only_changes: bool,
    pub goods: Vec<Goods>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Goods {
    pub id: String,
    pub deleted: bool,
    pub name: String,
    pub article: String,
    pub barcode: String,
    pub description: String,
    pub groups: Vec<String>,
    pub pictures: Vec<String>,
    pub manufacturer: Option<(String, String)>,
    /// (ид свойства, значения)
    pub properties: Vec<(String, Vec<String>)>,
    /// Реквизиты: (наименование, значение)
    pub traits: Vec<(String, String)>,
    /// Ставки налогов: (наименование, ставка)
    pub taxes: Vec<(String, String)>,
    pub base_unit: String,
    pub weight_grams: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OfferPackage {
    pub catalog_id: String,
    pub classifier_id: String,
    pub only_changes: bool,
    pub offers: Vec<Offer>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Price {
    pub type_id: String,
    pub value: f64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Offer {
    pub id: String,
    pub deleted: bool,
    pub name: String,
    /// `None` — в предложении нет блока `Цены`; `Some(пусто)` — блок есть, но пуст.
    pub prices: Option<Vec<Price>>,
    /// То же для `Остатки`: (ид склада, количество).
    pub rests: Option<Vec<(String, f64)>>,
    pub quantity: Option<f64>,
    pub characteristics: Vec<(String, String)>,
}

/// Разобрать один файл CommerceML (кодировка — по объявлению XML, BOM допускается).
pub fn parse(data: &[u8]) -> Result<Parsed, CmlError> {
    let mut reader = Reader::from_reader(data);
    let mut st = State::default();
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| CmlError::Xml(format!("{e} (позиция {})", reader.error_position())))?;
        let decoder = reader.decoder();
        match event {
            Event::Start(e) => {
                let (name, attrs) = element(&e, decoder)?;
                st.start(name, &attrs)?;
            }
            Event::Empty(e) => {
                let (name, attrs) = element(&e, decoder)?;
                st.start(name, &attrs)?;
                st.end()?;
            }
            Event::End(_) => st.end()?,
            Event::Text(t) => {
                let t = t.unescape().map_err(|e| CmlError::Xml(e.to_string()))?;
                st.text.push_str(&t);
            }
            Event::CData(c) => {
                let c = c.decode().map_err(|e| CmlError::Xml(e.to_string()))?;
                st.text.push_str(&c);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !st.seen_root {
        return Err(CmlError::NotCml);
    }
    if !st.stack.is_empty() {
        return Err(CmlError::Xml("неожиданный конец файла".into()));
    }
    Ok(st.out)
}

pub(crate) fn element(
    e: &BytesStart,
    decoder: Decoder,
) -> Result<(String, Vec<(String, String)>), CmlError> {
    let xml = |m: String| CmlError::Xml(m);
    let name = decoder
        .decode(e.local_name().as_ref())
        .map_err(|e| xml(e.to_string()))?
        .into_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| xml(e.to_string()))?;
        let key = decoder
            .decode(a.key.local_name().as_ref())
            .map_err(|e| xml(e.to_string()))?
            .into_owned();
        let value = a
            .decode_and_unescape_value(decoder)
            .map_err(|e| xml(e.to_string()))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok((name, attrs))
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

pub(crate) fn flag(s: &str) -> bool {
    let s = s.trim();
    s.eq_ignore_ascii_case("true") || s == "1" || s.eq_ignore_ascii_case("да")
}

/// Число из 1С: пробелы (в том числе неразрывные) отбрасываются, запятая — десятичный разделитель;
/// пустая строка — `None`, нечисловое и не конечное значение — ошибка.
pub(crate) fn number(s: &str) -> Result<Option<f64>, CmlError> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\u{a0}' && *c != '\u{202f}')
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    if cleaned.is_empty() {
        return Ok(None);
    }
    match cleaned.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(Some(v)),
        _ => Err(CmlError::Number(s.trim().to_string())),
    }
}

#[derive(Default)]
struct State {
    stack: Vec<String>,
    text: String,
    seen_root: bool,
    out: Parsed,
    /// Классификатор создан для `ТипыЦен`/`Склады` пакета предложений (2.x).
    synthetic_classifier: bool,
    package_name: String,
    /// Индексы разворачиваемых групп (стек вложенности).
    group_idx: Vec<usize>,
    prop: Property,
    option: (String, String),
    unit_id: String,
    unit: Unit,
    store: Store,
    price_type: PriceType,
    goods: Goods,
    prop_id: String,
    prop_values: Vec<String>,
    manufacturer: (String, String),
    trait_item: (String, String),
    tax_item: (String, String),
    weight_element: Option<String>,
    weight_requisite: Option<String>,
    weight_unit: String,
    base_unit_code: String,
    offer: Offer,
    price: Option<f64>,
    price_item: Price,
    characteristic: (String, String),
    rest_store: String,
    rest_qty: Option<f64>,
}

impl State {
    fn parent(&self) -> &str {
        let n = self.stack.len();
        if n >= 2 { &self.stack[n - 2] } else { "" }
    }

    fn grandparent(&self) -> &str {
        let n = self.stack.len();
        if n >= 3 { &self.stack[n - 3] } else { "" }
    }

    fn classifier(&mut self) -> &mut Classifier {
        self.out.classifier.get_or_insert_with(Classifier::default)
    }

    fn start(&mut self, name: String, attrs: &[(String, String)]) -> Result<(), CmlError> {
        if self.stack.is_empty() {
            if name != "КоммерческаяИнформация" || self.seen_root {
                return Err(CmlError::NotCml);
            }
            self.seen_root = true;
        }
        self.text.clear();
        self.stack.push(name.clone());
        let parent = self.parent().to_string();
        let only_changes = attr(attrs, "СодержитТолькоИзменения").is_some_and(flag);
        match (name.as_str(), parent.as_str()) {
            ("Классификатор", "КоммерческаяИнформация") => {
                self.out.classifier = Some(Classifier {
                    only_changes,
                    ..Classifier::default()
                });
                self.synthetic_classifier = false;
            }
            ("Каталог", "КоммерческаяИнформация") => {
                self.out.catalog = Some(Catalog {
                    only_changes,
                    ..Catalog::default()
                });
            }
            ("ПакетПредложений", "КоммерческаяИнформация") => {
                self.out.offers = Some(OfferPackage {
                    only_changes,
                    ..OfferPackage::default()
                });
                if self.out.classifier.is_none() {
                    self.out.classifier = Some(Classifier {
                        only_changes,
                        ..Classifier::default()
                    });
                    self.synthetic_classifier = true;
                }
            }
            ("Группа", _) => {
                let parent_idx = self.group_idx.last().copied();
                let cls = self.classifier();
                let parent_id = parent_idx
                    .map(|i| cls.groups[i].id.clone())
                    .filter(|id| !id.is_empty());
                cls.groups.push(Group {
                    parent: parent_id,
                    ..Group::default()
                });
                let idx = cls.groups.len() - 1;
                self.group_idx.push(idx);
            }
            ("Свойство", "Свойства") => self.prop = Property::default(),
            ("Справочник", "ВариантыЗначений") => {
                self.option = Default::default()
            }
            ("ЕдиницаИзмерения", _) => {
                self.unit = Unit::default();
                self.unit_id.clear();
            }
            ("Склад", "Склады") => self.store = Store::default(),
            ("Склад", "Предложение" | "Остаток") => {
                self.rest_store.clear();
                self.rest_qty = None;
                if let Some(id) = attr(attrs, "ИдСклада") {
                    self.rest_store = id.trim().to_string();
                    self.rest_qty = number(attr(attrs, "КоличествоНаСкладе").unwrap_or(""))?;
                }
            }
            ("ТипЦены", "ТипыЦен") => self.price_type = PriceType::default(),
            ("Товар", "Товары") => {
                self.goods = Goods::default();
                self.weight_element = None;
                self.weight_requisite = None;
                self.weight_unit.clear();
            }
            ("БазоваяЕдиница", "Товар") => {
                self.base_unit_code = attr(attrs, "Код").unwrap_or("").trim().to_string();
            }
            ("Изготовитель", "Товар") => self.manufacturer = Default::default(),
            ("ЗначенияСвойства", "ЗначенияСвойств") => {
                self.prop_id.clear();
                self.prop_values.clear();
            }
            ("ЗначениеРеквизита", "ЗначенияРеквизитов") => {
                self.trait_item = Default::default()
            }
            ("СтавкаНалога", "СтавкиНалогов") => {
                self.tax_item = Default::default()
            }
            ("Предложение", "Предложения") => self.offer = Offer::default(),
            ("Цены", "Предложение") => self.offer.prices = Some(Vec::new()),
            ("Остатки", "Предложение") => self.offer.rests = Some(Vec::new()),
            ("Цена", "Цены") => {
                self.price = None;
                self.price_item = Price::default();
            }
            ("ХарактеристикаТовара", "ХарактеристикиТовара") => {
                self.characteristic = Default::default()
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), CmlError> {
        let Some(name) = self.stack.last().cloned() else {
            return Err(CmlError::NotCml);
        };
        let text = std::mem::take(&mut self.text);
        let id = text.trim().to_string();
        let parent = self.parent().to_string();
        let grand = self.grandparent().to_string();
        match (name.as_str(), parent.as_str()) {
            // Классификатор
            ("Ид", "Классификатор") => self.classifier().id = id,
            ("Наименование", "Классификатор") => {
                self.classifier().name = text
            }
            ("Ид", "Группа") => self.cur_group()?.id = id,
            ("Наименование", "Группа") => self.cur_group()?.name = text,
            ("ПометкаУдаления", "Группа") => {
                self.cur_group()?.deleted = flag(&text)
            }
            ("Группа", _) => {
                self.group_idx.pop();
            }
            ("Ид", "Свойство") => self.prop.id = id,
            ("Наименование", "Свойство") => self.prop.name = text,
            ("ПометкаУдаления", "Свойство") => {
                self.prop.deleted = flag(&text)
            }
            ("Множественное", "Свойство") => self.prop.multiple = flag(&text),
            ("ТипЗначений", "Свойство") => {
                self.prop.kind = match id.as_str() {
                    "Справочник" => PropKind::Directory,
                    "Число" => PropKind::Number,
                    "Дата" => PropKind::Date,
                    _ => PropKind::String,
                }
            }
            ("ИдЗначения", "Справочник") => self.option.0 = id,
            ("Значение", "Справочник") => self.option.1 = text,
            ("Справочник", "ВариантыЗначений") => {
                let option = std::mem::take(&mut self.option);
                self.prop.options.push(option);
            }
            ("Свойство", "Свойства") => {
                let prop = std::mem::take(&mut self.prop);
                self.classifier().properties.push(prop);
            }
            ("Ид", "ЕдиницаИзмерения") => self.unit_id = id,
            ("Код", "ЕдиницаИзмерения") => self.unit.code = id,
            ("НаименованиеКраткое", "ЕдиницаИзмерения") => {
                self.unit.name = text.trim().to_string()
            }
            ("ЕдиницаИзмерения", "ЕдиницыИзмерения") => {
                let mut unit = std::mem::take(&mut self.unit);
                if unit.code.is_empty() {
                    unit.code = std::mem::take(&mut self.unit_id);
                }
                self.classifier().units.push(unit);
            }
            ("Ид", "Склад") if grand == "Склады" => self.store.id = id,
            ("Наименование", "Склад") if grand == "Склады" => {
                self.store.name = text
            }
            ("ПометкаУдаления", "Склад") if grand == "Склады" => {
                self.store.deleted = flag(&text)
            }
            ("Склад", "Склады") => {
                let store = std::mem::take(&mut self.store);
                self.classifier().stores.push(store);
            }
            ("Ид", "ТипЦены") => self.price_type.id = id,
            ("Наименование", "ТипЦены") => self.price_type.name = text,
            ("Валюта", "ТипЦены") => self.price_type.currency = id,
            ("ПометкаУдаления", "ТипЦены") => {
                self.price_type.deleted = flag(&text)
            }
            ("ТипЦены", "ТипыЦен") => {
                let price_type = std::mem::take(&mut self.price_type);
                self.classifier().price_types.push(price_type);
            }
            ("Классификатор", "КоммерческаяИнформация") => {}

            // Каталог и товары
            ("Ид", "Каталог") => self.catalog()?.id = id,
            ("ИдКлассификатора", "Каталог") => {
                self.catalog()?.classifier_id = id
            }
            ("Наименование", "Каталог") => self.catalog()?.name = text,
            ("Ид", "Товар") => self.goods.id = id,
            ("ПометкаУдаления", "Товар") => self.goods.deleted |= flag(&text),
            ("Статус", "Товар") => self.goods.deleted |= id == "Удален",
            ("Штрихкод", "Товар") => self.goods.barcode = id,
            ("Артикул", "Товар") => self.goods.article = id,
            ("Наименование", "Товар") => self.goods.name = text,
            ("БазоваяЕдиница", "Товар") => {
                // 2.x: код единицы лежит в атрибуте, 3.1 — в тексте.
                let code = std::mem::take(&mut self.base_unit_code);
                self.goods.base_unit = if code.is_empty() { id } else { code };
            }
            ("Описание", "Товар") => self.goods.description = text,
            ("Картинка", "Товар") => self.goods.pictures.push(id),
            ("Вес", "Товар") => self.weight_element = Some(id),
            ("Ид", "Группы") if grand == "Товар" => self.goods.groups.push(id),
            ("Ид", "Изготовитель") => self.manufacturer.0 = id,
            ("Наименование", "Изготовитель") => self.manufacturer.1 = text,
            ("Изготовитель", "Товар") => {
                let m = std::mem::take(&mut self.manufacturer);
                self.goods.manufacturer = Some(m);
            }
            ("Ид", "ЗначенияСвойства") => self.prop_id = id,
            ("Значение", "ЗначенияСвойства" | "ЗначениеСвойства") => {
                self.prop_values.push(text)
            }
            ("ЗначенияСвойства", "ЗначенияСвойств") => {
                let values = std::mem::take(&mut self.prop_values);
                let prop_id = std::mem::take(&mut self.prop_id);
                if !prop_id.is_empty() {
                    self.goods.properties.push((prop_id, values));
                }
            }
            ("Наименование", "ЗначениеРеквизита") => {
                self.trait_item.0 = text
            }
            ("Значение", "ЗначениеРеквизита") => self.trait_item.1 = text,
            ("ЗначениеРеквизита", "ЗначенияРеквизитов") if grand == "Товар" =>
            {
                let (key, value) = std::mem::take(&mut self.trait_item);
                match key.trim() {
                    "Вес" => self.weight_requisite = Some(value.trim().to_string()),
                    "ЕдиницаВеса" => self.weight_unit = value.trim().to_lowercase(),
                    _ => {}
                }
                self.goods.traits.push((key, value));
            }
            ("Наименование", "СтавкаНалога") => self.tax_item.0 = text,
            ("Ставка", "СтавкаНалога") => self.tax_item.1 = id,
            ("СтавкаНалога", "СтавкиНалогов") => {
                let tax = std::mem::take(&mut self.tax_item);
                self.goods.taxes.push(tax);
            }
            ("Товар", "Товары") => {
                let mut goods = std::mem::take(&mut self.goods);
                goods.weight_grams = self.weight_grams();
                self.catalog()?.goods.push(goods);
            }

            // Пакет предложений
            ("ИдКаталога", "ПакетПредложений") => {
                self.package()?.catalog_id = id
            }
            ("ИдКлассификатора", "ПакетПредложений") => {
                self.package()?.classifier_id = id
            }
            ("Наименование", "ПакетПредложений") => {
                self.package_name = text
            }
            ("ПакетПредложений", "КоммерческаяИнформация") if self.synthetic_classifier =>
            {
                let classifier_id = self.package()?.classifier_id.clone();
                let name = std::mem::take(&mut self.package_name);
                let cls = self.classifier();
                if cls.price_types.is_empty() && cls.stores.is_empty() {
                    self.out.classifier = None;
                } else {
                    cls.id = classifier_id;
                    cls.name = name;
                }
                self.synthetic_classifier = false;
            }
            ("Ид", "Предложение") => self.offer.id = id,
            ("ПометкаУдаления", "Предложение") => {
                self.offer.deleted |= flag(&text)
            }
            ("Статус", "Предложение") => self.offer.deleted |= id == "Удален",
            ("Наименование", "Предложение") => self.offer.name = text,
            ("Количество", "Предложение") => {
                self.offer.quantity = number(&text)?
            }
            ("ИдТипаЦены", "Цена") => self.price_item.type_id = id,
            ("ЦенаЗаЕдиницу", "Цена") => self.price = number(&text)?,
            ("Валюта", "Цена") => self.price_item.currency = id,
            ("Цена", "Цены") => {
                if let Some(value) = self.price.take() {
                    let mut price = std::mem::take(&mut self.price_item);
                    price.value = value;
                    self.offer.prices.get_or_insert_with(Vec::new).push(price);
                }
            }
            ("Наименование", "ХарактеристикаТовара") => {
                self.characteristic.0 = text
            }
            ("Значение", "ХарактеристикаТовара") => {
                self.characteristic.1 = text
            }
            ("ХарактеристикаТовара", "ХарактеристикиТовара") =>
            {
                let c = std::mem::take(&mut self.characteristic);
                self.offer.characteristics.push(c);
            }
            ("Ид", "Склад") if grand == "Остаток" => self.rest_store = id,
            ("Количество", "Склад") if grand == "Остаток" => {
                self.rest_qty = number(&text)?
            }
            ("Склад", "Остаток" | "Предложение") => {
                let store = std::mem::take(&mut self.rest_store);
                let qty = self.rest_qty.take().unwrap_or(0.0);
                if !store.is_empty() {
                    self.offer
                        .rests
                        .get_or_insert_with(Vec::new)
                        .push((store, qty));
                }
            }
            ("Предложение", "Предложения") => {
                let offer = std::mem::take(&mut self.offer);
                self.package()?.offers.push(offer);
            }
            _ => {}
        }
        self.stack.pop();
        Ok(())
    }

    fn cur_group(&mut self) -> Result<&mut Group, CmlError> {
        let idx = *self.group_idx.last().ok_or(CmlError::NotCml)?;
        Ok(&mut self.classifier().groups[idx])
    }

    fn catalog(&mut self) -> Result<&mut Catalog, CmlError> {
        self.out.catalog.as_mut().ok_or(CmlError::NotCml)
    }

    fn package(&mut self) -> Result<&mut OfferPackage, CmlError> {
        self.out.offers.as_mut().ok_or(CmlError::NotCml)
    }

    /// Вес товара в граммах: «Вес» (элемент или реквизит) в 1С хранится в килограммах, как и в Битриксе;
    /// граммы берутся как есть только если реквизит «ЕдиницаВеса» явно называет граммы (грамм, «г», код ОКЕИ 163),
    /// миллиграммы («мг», код 161) делятся на 1000.
    fn weight_grams(&self) -> Option<f64> {
        let raw = self
            .weight_requisite
            .as_ref()
            .or(self.weight_element.as_ref())?;
        let value = number(raw).ok().flatten()?;
        let unit = self.weight_unit.as_str();
        // Сначала кило- и миллиграммы (оба содержат «грамм»), затем граммы;
        // неизвестная единица — килограммы.
        let kilograms = unit.contains("кг") || unit.starts_with("килограмм") || unit == "166";
        let milligrams =
            !kilograms && (unit == "мг" || unit.starts_with("миллиграмм") || unit == "161");
        let grams =
            !kilograms && !milligrams && (unit.contains("грамм") || unit == "г" || unit == "163");
        Some(if milligrams {
            value / 1000.0
        } else if grams {
            value
        } else {
            value * 1000.0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(bytes: &[u8]) -> Parsed {
        parse(bytes).expect("разбор")
    }

    fn classifier(bytes: &[u8]) -> Classifier {
        load(bytes).classifier.expect("классификатор")
    }

    #[test]
    fn parses_groups_tree() {
        let c = classifier(include_bytes!("../../tests/fixtures/1c/groups.xml"));
        assert_eq!(c.id, "cls-0001");
        assert_eq!(c.name, "Тестовый каталог");
        assert!(c.only_changes);
        let g: Vec<_> = c
            .groups
            .iter()
            .map(|g| (g.id.as_str(), g.parent.as_deref(), g.deleted))
            .collect();
        assert_eq!(
            g,
            vec![
                ("g-root", None, false),
                ("g-child", Some("g-root"), false),
                ("g-grand", Some("g-child"), true),
                ("g-child2", Some("g-root"), false),
                ("g-root2", None, false),
            ]
        );
        assert_eq!(c.groups[0].name, "Запчасти & компоненты");
        assert_eq!(c.groups[1].parent, Some("g-root".to_string()));
    }

    #[test]
    fn parses_properties() {
        let c = classifier(include_bytes!("../../tests/fixtures/1c/properties.xml"));
        assert_eq!(c.properties.len(), 4);
        let p = &c.properties[0];
        assert_eq!(p.id, "p-dir");
        assert_eq!(p.name, "Применимость");
        assert_eq!(p.kind, PropKind::Directory);
        assert!(!p.multiple && !p.deleted);
        assert_eq!(
            p.options[0],
            ("v-1".to_string(), " Для автомобилей ".to_string())
        );
        assert_eq!(p.options.len(), 2);
        let s = &c.properties[1];
        assert_eq!(
            (s.kind, s.multiple, s.deleted),
            (PropKind::String, true, true)
        );
        assert!(s.options.is_empty());
        assert_eq!(c.properties[2].kind, PropKind::Number);
        assert_eq!(c.properties[3].kind, PropKind::Date);
    }

    #[test]
    fn parses_units_with_trimmed_codes() {
        let c = classifier(include_bytes!("../../tests/fixtures/1c/units.xml"));
        assert_eq!(
            c.units,
            vec![
                Unit {
                    code: "796".into(),
                    name: "шт".into()
                },
                Unit {
                    code: "006".into(),
                    name: "м".into()
                },
            ]
        );
    }

    #[test]
    fn parses_storages() {
        let c = classifier(include_bytes!("../../tests/fixtures/1c/storages.xml"));
        assert_eq!(c.stores.len(), 2);
        assert_eq!(
            c.stores[0],
            Store {
                id: "s-1".into(),
                name: "Центральный".into(),
                deleted: false
            }
        );
        assert!(c.stores[1].deleted);
    }

    #[test]
    fn parses_price_lists() {
        let c = classifier(include_bytes!("../../tests/fixtures/1c/price_lists.xml"));
        assert_eq!(c.price_types.len(), 2);
        assert_eq!(
            c.price_types[0],
            PriceType {
                id: "pt-1".into(),
                name: "Розничный".into(),
                currency: "RUB".into(),
                deleted: false
            }
        );
        assert!(c.price_types[1].deleted);
        assert_eq!(c.price_types[1].currency, "USD");
    }

    #[test]
    fn catalog_header_next_to_classifier() {
        let p = load(include_bytes!("../../tests/fixtures/1c/groups.xml"));
        let cat = p.catalog.expect("каталог");
        assert_eq!(cat.id, "cat-0001");
        assert_eq!(cat.classifier_id, "cls-0001");
        assert!(cat.only_changes);
        assert!(cat.goods.is_empty());
    }

    #[test]
    fn parses_goods() {
        let p = load(include_bytes!("../../tests/fixtures/1c/goods.xml"));
        assert!(p.classifier.is_none());
        let cat = p.catalog.expect("каталог");
        assert!(!cat.only_changes);
        assert_eq!(cat.goods.len(), 2);
        let g = &cat.goods[0];
        assert_eq!(g.id, "good-1");
        assert!(!g.deleted);
        assert_eq!(g.barcode, "4600000000017");
        assert_eq!(g.article, "ART-100");
        assert_eq!(g.name, "Ремкомплект тестовый");
        assert_eq!(g.base_unit, "796");
        assert_eq!(g.groups, vec!["g-child", "g-root2"]);
        assert_eq!(
            g.description,
            "Первая строка\n\tвторая строка с табом & амперсандом\nТретья"
        );
        assert_eq!(g.pictures.len(), 2);
        assert_eq!(g.pictures[1], "import_files/aa/good-1_b.jpeg");
        assert_eq!(
            g.manufacturer,
            Some(("m-1".to_string(), "ACDelco".to_string()))
        );
        assert_eq!(g.properties.len(), 4);
        assert_eq!(
            g.properties[0],
            ("p-dir".to_string(), vec!["v-1".to_string()])
        );
        assert_eq!(g.properties[1].1, vec!["красный", "синий"]);
        assert_eq!(g.properties[2], ("p-num".to_string(), vec![String::new()]));
        assert_eq!(g.properties[3].1, vec!["Строка один\nСтрока два\tс табом"]);
        assert_eq!(g.taxes, vec![("НДС".to_string(), "20".to_string())]);
        assert_eq!(g.traits.len(), 3);
        assert_eq!(
            g.traits[0],
            ("ВидНоменклатуры".to_string(), "Запчасти".to_string())
        );
        assert_eq!(g.traits[2], ("Планируемая дата".to_string(), String::new()));
        assert_eq!(g.weight_grams, Some(1250.0));

        let d = &cat.goods[1];
        assert!(d.deleted);
        assert_eq!(d.barcode, "");
        assert_eq!(d.description, "");
        assert_eq!(d.base_unit, "006");
        assert_eq!(d.manufacturer, None);
        assert_eq!(d.weight_grams, None);
        assert!(d.pictures.is_empty() && d.properties.is_empty());
    }

    #[test]
    fn parses_prices() {
        let p = load(include_bytes!("../../tests/fixtures/1c/prices.xml"));
        assert!(p.catalog.is_none() && p.classifier.is_none());
        let pkg = p.offers.expect("пакет");
        assert_eq!(pkg.catalog_id, "cat-0001");
        assert_eq!(pkg.classifier_id, "cls-0001");
        assert!(pkg.only_changes);
        assert_eq!(pkg.offers.len(), 2);
        let prices = pkg.offers[0].prices.clone().unwrap();
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[0].value, 6540.66);
        assert_eq!(prices[0].type_id, "pt-1");
        assert_eq!(prices[0].currency, "RUB");
        assert_eq!(prices[1].value, 80.0);
        assert_eq!(pkg.offers[0].rests, None);
        assert_eq!(pkg.offers[1].prices, Some(vec![]));
    }

    #[test]
    fn parses_rests() {
        let pkg = load(include_bytes!("../../tests/fixtures/1c/rests.xml"))
            .offers
            .expect("пакет");
        assert_eq!(
            pkg.offers[0].rests,
            Some(vec![("s-1".to_string(), 6.0), ("s-2".to_string(), 2.5)])
        );
        assert_eq!(pkg.offers[0].prices, None);
        assert_eq!(pkg.offers[1].rests, Some(vec![]));
        assert_eq!(pkg.offers[2].rests, None);
        assert_eq!(pkg.offers[2].id, "good-3");
    }

    #[test]
    fn detects_combined_import_xml() {
        let p = load(include_bytes!("../../tests/fixtures/1c/import.xml"));
        let c = p.classifier.expect("классификатор");
        let cat = p.catalog.expect("каталог");
        assert!(p.offers.is_none());
        assert_eq!(c.id, "cls-2x");
        assert!(!c.only_changes);
        assert_eq!(c.groups.len(), 2);
        assert_eq!(c.groups[1].parent.as_deref(), Some("g2-root"));
        assert_eq!(
            c.properties[0].options[0],
            ("v2-1".to_string(), "XL".to_string())
        );
        assert_eq!(cat.id, "cat-2x");
        let g = &cat.goods[0];
        assert_eq!(g.id, "g2-good");
        assert_eq!(g.base_unit, "796");
        assert_eq!(g.weight_grams, Some(300.0));
        assert_eq!(g.properties[0].1, vec!["v2-1"]);
    }

    #[test]
    fn parses_2x_offers_with_classifier_parts() {
        let p = load(include_bytes!("../../tests/fixtures/1c/offers.xml"));
        let c = p.classifier.expect("типы цен и склады пакета");
        assert_eq!(c.id, "cls-2x");
        assert_eq!(c.price_types[0].id, "pt2-1");
        assert_eq!(c.price_types[0].currency, "RUB");
        assert_eq!(
            c.stores[0],
            Store {
                id: "st2-1".into(),
                name: "Основной".into(),
                deleted: false
            }
        );
        let pkg = p.offers.expect("пакет");
        assert!(!pkg.only_changes);
        let o = &pkg.offers[0];
        assert_eq!(o.id, "g2-good#sku-red-m");
        assert_eq!(o.name, "Футболка (красная, M)");
        assert_eq!(
            o.characteristics,
            vec![
                ("Цвет".to_string(), "красный".to_string()),
                ("Размер".to_string(), "M".to_string())
            ]
        );
        assert_eq!(o.prices.as_ref().unwrap()[0].value, 999.5);
        assert_eq!(o.quantity, Some(7.0));
        assert_eq!(o.rests, Some(vec![("st2-1".to_string(), 4.0)]));
    }

    #[test]
    fn cp1251_decoded() {
        let bytes = include_bytes!("../../tests/fixtures/1c/goods_cp1251.xml");
        assert!(String::from_utf8(bytes.to_vec()).is_err());
        let g = &load(bytes).catalog.unwrap().goods[0];
        assert_eq!(g.article, "Ё-1");
        assert_eq!(g.name, "Товар в кодировке 1251");
        assert_eq!(g.description, "Описание & ещё");
    }

    #[test]
    fn bom_ok() {
        let with_bom = include_bytes!("../../tests/fixtures/1c/groups.xml");
        assert_eq!(&with_bom[..3], b"\xEF\xBB\xBF");
        let without = &with_bom[3..];
        assert_eq!(load(with_bom), load(without));
        assert_eq!(load(without).classifier.unwrap().groups.len(), 5);
    }

    #[test]
    fn garbage_is_error() {
        assert!(parse(b"").is_err());
        assert!(parse(b"just text, not xml").is_err());
        assert!(matches!(
            parse(b"<html><body/></html>"),
            Err(CmlError::NotCml)
        ));
        assert!(matches!(
            parse("<КоммерческаяИнформация><Каталог><Ид>x</Каталог>".as_bytes()),
            Err(CmlError::Xml(_))
        ));
        let msg = parse(b"<a/>").unwrap_err().to_string();
        assert!(msg.contains("CommerceML"));
    }

    fn weight_of(raw: &str, unit: &str) -> Option<f64> {
        let st = State {
            weight_requisite: Some(raw.to_string()),
            weight_unit: unit.to_string(),
            ..State::default()
        };
        st.weight_grams()
    }

    #[test]
    fn weight_units() {
        for unit in ["", "кг", "килограмм", "килограммы", "166", "неизвестно"]
        {
            assert_eq!(weight_of("1,5", unit), Some(1500.0), "{unit}");
        }
        for unit in ["г", "грамм", "граммы", "163"] {
            assert_eq!(weight_of("1,5", unit), Some(1.5), "{unit}");
        }
        for unit in ["мг", "миллиграмм", "миллиграммы", "161"] {
            assert_eq!(weight_of("1500", unit), Some(1.5), "{unit}");
        }
    }

    fn goods_xml(inner: &str) -> Vec<u8> {
        format!(
            "<КоммерческаяИнформация><Каталог><Товары><Товар><Ид>x</Ид>{inner}</Товар></Товары></Каталог></КоммерческаяИнформация>"
        )
        .into_bytes()
    }

    #[test]
    fn cdata_and_entities_in_text() {
        let p = load(&goods_xml(
            "<Наименование>Масло &quot;Люкс&quot; &lt;5W&gt;</Наименование>\
             <Описание><![CDATA[<b>жирный</b> & текст]]></Описание>",
        ));
        let g = &p.catalog.unwrap().goods[0];
        assert_eq!(g.name, "Масло \"Люкс\" <5W>");
        assert_eq!(g.description, "<b>жирный</b> & текст");
    }

    #[test]
    fn invalid_number_is_error() {
        let xml = "<КоммерческаяИнформация><ПакетПредложений><Предложения><Предложение>\
                   <Ид>x</Ид><Количество>много</Количество></Предложение></Предложения>\
                   </ПакетПредложений></КоммерческаяИнформация>";
        match parse(xml.as_bytes()) {
            Err(CmlError::Number(n)) => assert_eq!(n, "много"),
            other => panic!("ожидалась ошибка числа: {other:?}"),
        }
    }

    #[test]
    fn weight_requisite_wins_over_element() {
        let weight = |inner: &str| load(&goods_xml(inner)).catalog.unwrap().goods[0].weight_grams;
        assert_eq!(weight("<Вес>2</Вес>"), Some(2000.0));
        assert_eq!(
            weight(
                "<Вес>2</Вес><ЗначенияРеквизитов><ЗначениеРеквизита><Наименование>Вес</Наименование>\
                 <Значение>0,5</Значение></ЗначениеРеквизита></ЗначенияРеквизитов>"
            ),
            Some(500.0)
        );
        assert_eq!(weight(""), None);
    }

    #[test]
    fn store_attribute_without_quantity_is_zero() {
        let xml = r#"<КоммерческаяИнформация><ПакетПредложений><Предложения><Предложение><Ид>x</Ид><Склад ИдСклада="st-1"/><Склад ИдСклада="st-2" КоличествоНаСкладе="3"/></Предложение></Предложения></ПакетПредложений></КоммерческаяИнформация>"#;
        let o = &load(xml.as_bytes()).offers.unwrap().offers[0];
        assert_eq!(
            o.rests,
            Some(vec![("st-1".to_string(), 0.0), ("st-2".to_string(), 3.0)])
        );
    }

    #[test]
    fn number_formats() {
        assert_eq!(number("6 540,66").unwrap(), Some(6540.66));
        assert_eq!(number("6\u{a0}540.66").unwrap(), Some(6540.66));
        assert_eq!(number("1\u{202f}000").unwrap(), Some(1000.0));
        assert_eq!(number(" ").unwrap(), None);
        for bad in ["NaN", "inf", "-inf", "infinity", "abc"] {
            assert!(matches!(number(bad), Err(CmlError::Number(_))), "{bad}");
        }
    }

    #[test]
    fn base_unit_prefers_code_attribute() {
        let xml = |inner: &str| {
            format!(
                "<КоммерческаяИнформация><Каталог><Товары><Товар><Ид>x</Ид>{inner}</Товар></Товары></Каталог></КоммерческаяИнформация>"
            )
        };
        let unit = |inner: &str| {
            let p = parse(xml(inner).as_bytes()).unwrap();
            p.catalog.unwrap().goods[0].base_unit.clone()
        };
        assert_eq!(
            unit(r#"<БазоваяЕдиница Код=" 796 ">шт</БазоваяЕдиница>"#),
            "796"
        );
        assert_eq!(unit(r#"<БазоваяЕдиница Код="796"/>"#), "796");
        assert_eq!(unit("<БазоваяЕдиница>796 </БазоваяЕдиница>"), "796");
        assert_eq!(unit("<БазоваяЕдиница>шт</БазоваяЕдиница>"), "шт");
    }
}
