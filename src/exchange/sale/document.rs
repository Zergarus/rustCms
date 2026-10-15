//! Документ заказа для выгрузки в 1С (CommerceML, `CSaleExport` из `sale/general/export.php`):
//! модель без БД и запись XML в windows-1251.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, Utc};
use encoding_rs::EncoderResult;
use md5::{Digest, Md5};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use super::settings::Domain;

/// Версия схемы обмена (`CSaleExport::getVersionSchema`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SchemaVersion(pub f64);

impl Default for SchemaVersion {
    fn default() -> Self {
        SchemaVersion(2.05)
    }
}

impl SchemaVersion {
    /// Число из `version` 1С: запятая — точка, пробелы убираются; пусто — 2.05; нечисловое — по
    /// числовому началу, как `doubleval`.
    pub fn parse(raw: Option<&str>) -> SchemaVersion {
        let cleaned: String = raw
            .unwrap_or("")
            .chars()
            .filter(|c| *c != ' ')
            .map(|c| if c == ',' { '.' } else { c })
            .collect();
        if cleaned.is_empty() || cleaned == "0" {
            return SchemaVersion::default();
        }
        SchemaVersion(leading_number(&cleaned))
    }

    /// Больше 2.05: НомерВерсии, Номер1С, `Единица` с кодом, новые типы контактов.
    pub fn new_contacts(&self) -> bool {
        self.0 > DEFAULT_VERSION
    }

    /// От 2.10: документы оплат, отгрузок и удалённых отгрузок.
    pub fn documents(&self) -> bool {
        self.0 >= PARTIAL_VERSION
    }

    /// От 3: документы заказа в `<Контейнер>`.
    pub fn container(&self) -> bool {
        self.0 >= CONTAINER_VERSION
    }
}

const DEFAULT_VERSION: f64 = 2.05;
const PARTIAL_VERSION: f64 = 2.1;
const CONTAINER_VERSION: f64 = 3.0;

/// Числовое начало строки (`doubleval("2.1abc") == 2.1`, `doubleval("abc") == 0`).
fn leading_number(s: &str) -> f64 {
    let mut end = 0;
    let mut dot = false;
    for (i, c) in s.char_indices() {
        let ok = c.is_ascii_digit() || (c == '.' && !dot) || (i == 0 && (c == '-' || c == '+'));
        if !ok {
            break;
        }
        dot |= c == '.';
        end = i + c.len_utf8();
    }
    s[..end].parse().unwrap_or(0.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportOrder {
    pub id: i64,
    pub number: String,
    pub created_at: DateTime<Utc>,
    pub currency: String,
    pub sum: f64,
    pub comment: String,
    pub id_1c: String,
    pub version_1c: String,
    pub agent: Agent,
    pub items: Vec<ExportItem>,
    pub delivery: Option<ExportDelivery>,
    pub store_xml_id: Option<String>,
    pub traits: Vec<(String, String)>,
    pub payments: Vec<ExportPayment>,
    pub shipments: Vec<ExportShipment>,
    pub removed_shipments: Vec<RemovedShipment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemovedShipment {
    /// `id_1c` отгрузки или наш id.
    pub id: String,
    pub date: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub id: String,
    pub domain: Domain,
    /// Ключи — поля контрагента `$systemCodes` (FULL_NAME, INN, PHONE…).
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportItem {
    pub xml_id: String,
    pub catalog_xml_id: String,
    pub name: String,
    pub unit_code: String,
    pub unit_name: String,
    pub price: f64,
    pub quantity: f64,
    pub sum: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDelivery {
    pub price: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportPayment {
    pub id: String,
    pub number: String,
    pub date: DateTime<Utc>,
    pub operation: PayOperation,
    pub sum: f64,
    pub version_1c: String,
    pub traits: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayOperation {
    Cashless,
    Cash,
    Acquiring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportShipment {
    pub id: String,
    pub number: String,
    pub date: DateTime<Utc>,
    pub sum: f64,
    pub version_1c: String,
    pub items: Vec<ExportItem>,
    pub delivery: Option<ExportDelivery>,
    pub traits: Vec<(String, String)>,
}

impl PayOperation {
    /// `SALE_EXPORT_ITEM_PAYMENT_N|Y|A` по типу платёжной системы.
    fn operation(self) -> &'static str {
        match self {
            PayOperation::Cashless => "Выплата безналичных денег",
            PayOperation::Cash => "Выплата наличных денег",
            PayOperation::Acquiring => "Эквайринговая операция",
        }
    }
}

/// Выгрузка заказов (`CSaleExport::ExportOrders2Xml`): корень, за каждым заказом — от 2.10
/// документы оплат, отгрузок и удалённых отгрузок, от 3 — всё это в `<Контейнер>`.
/// Строится в UTF-8 и перекодируется в windows-1251, непредставимые символы — `?`.
pub fn render(
    orders: &[ExportOrder],
    version: SchemaVersion,
    generated_at: DateTime<Utc>,
) -> Vec<u8> {
    let mut x = Xml::new();
    x.decl();
    let schema = version.0.to_string();
    let generated = site_time(generated_at)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    x.open_attrs(
        "КоммерческаяИнформация",
        &[
            ("ВерсияСхемы", &schema),
            ("ДатаФормирования", &generated),
            ("ФорматДаты", "ДФ=yyyy-MM-dd; ДЛФ=DT"),
            ("ФорматВремени", "ДФ=ЧЧ:мм:сс; ДЛФ=T"),
            ("РазделительДатаВремя", "T"),
            ("ФорматСуммы", "ЧЦ=18; ЧДЦ=2; ЧРД=."),
            ("ФорматКоличества", "ЧЦ=18; ЧДЦ=2; ЧРД=."),
        ],
    );
    for o in orders {
        if version.container() {
            x.open("Контейнер");
        }
        order_document(&mut x, o, version);
        if version.documents() {
            for p in &o.payments {
                payment_document(&mut x, o, p, version);
            }
            for s in &o.shipments {
                shipment_document(&mut x, o, s, version);
            }
            for r in &o.removed_shipments {
                removed_shipment_document(&mut x, o, r);
            }
        }
        if version.container() {
            x.close("Контейнер");
        }
    }
    x.close("КоммерческаяИнформация");
    to_cp1251(&x.finish())
}

/// Время сайта (Москва): Битрикс пишет даты документов в часовом поясе сервера.
fn site_time(dt: DateTime<Utc>) -> DateTime<FixedOffset> {
    dt.with_timezone(&FixedOffset::east_opt(3 * 3600).expect("valid offset"))
}

fn date(dt: DateTime<Utc>) -> String {
    site_time(dt).format("%Y-%m-%d").to_string()
}

fn time(dt: DateTime<Utc>) -> String {
    site_time(dt).format("%H:%M:%S").to_string()
}

/// Число, как его печатает PHP: без лишних нулей (`1500`, `33.3`), с точностью до 4 знаков.
fn num(v: f64) -> String {
    let r = (v * 10_000.0).round() / 10_000.0;
    if r == 0.0 { "0".into() } else { r.to_string() }
}

/// `НомерВерсии`: `intval(VERSION) > 0 ? VERSION : 0`.
fn version_number(v: &str) -> &str {
    let v = v.trim();
    if leading_number(v) >= 1.0 { v } else { "0" }
}

/// `mb_substr(CURRENCY, 0, 3)`.
fn currency(c: &str) -> String {
    c.chars().take(3).collect()
}

fn order_document(x: &mut Xml, o: &ExportOrder, v: SchemaVersion) {
    x.open("Документ");
    x.leaf("Ид", &o.id.to_string());
    x.leaf("Номер", &o.number);
    x.leaf("Дата", &date(o.created_at));
    x.leaf("ХозОперация", "Заказ товара");
    x.leaf("Роль", "Продавец");
    x.leaf("Валюта", &currency(&o.currency));
    x.leaf("Курс", "1");
    x.leaf("Сумма", &num(o.sum));
    if v.new_contacts() {
        x.leaf("НомерВерсии", version_number(&o.version_1c));
        if !o.id_1c.is_empty() {
            x.leaf("Номер1С", &o.id_1c);
        }
    }
    contragents(x, &o.agent, v);
    x.leaf("Время", &time(o.created_at));
    x.leaf("Комментарий", &o.comment);
    if let Some(store) = o.store_xml_id.as_deref().filter(|s| !s.is_empty()) {
        x.open("Склады");
        x.open("Склад");
        x.leaf("Ид", store);
        x.close("Склад");
        x.close("Склады");
    }
    goods(x, &o.items, o.delivery.as_ref(), v);
    traits(x, &o.traits);
    x.close("Документ");
}

/// Начало документа оплаты или отгрузки: от Ид до Основания (`OutputXmlDocument`).
#[allow(clippy::too_many_arguments)]
fn sub_document_head(
    x: &mut Xml,
    o: &ExportOrder,
    id: &str,
    number: &str,
    at: DateTime<Utc>,
    operation: &str,
    sum: f64,
    version_1c: &str,
) {
    x.open("Документ");
    x.leaf("Ид", id);
    x.leaf("Номер", number);
    x.leaf("Дата", &date(at));
    x.leaf("ХозОперация", operation);
    x.leaf("Роль", "Продавец");
    x.leaf("Валюта", &currency(&o.currency));
    x.leaf("Курс", "1");
    x.leaf("Сумма", &num(sum));
    x.leaf("НомерВерсии", version_number(version_1c));
    x.leaf("Основание", &o.id.to_string());
}

fn payment_document(x: &mut Xml, o: &ExportOrder, p: &ExportPayment, v: SchemaVersion) {
    sub_document_head(
        x,
        o,
        &p.id,
        &p.number,
        p.date,
        p.operation.operation(),
        p.sum,
        &p.version_1c,
    );
    contragents(x, &o.agent, v);
    x.leaf("Время", &time(p.date));
    x.leaf("Комментарий", "");
    traits(x, &p.traits);
    x.close("Документ");
}

fn shipment_document(x: &mut Xml, o: &ExportOrder, s: &ExportShipment, v: SchemaVersion) {
    sub_document_head(
        x,
        o,
        &s.id,
        &s.number,
        s.date,
        "Отпуск товара",
        s.sum,
        &s.version_1c,
    );
    contragents(x, &o.agent, v);
    x.leaf("Время", &time(s.date));
    x.leaf("Комментарий", "");
    goods(x, &s.items, s.delivery.as_ref(), v);
    traits(x, &s.traits);
    x.close("Документ");
}

/// Документ удалённой отгрузки (`OutputXmlDocumentRemove`).
fn removed_shipment_document(x: &mut Xml, o: &ExportOrder, r: &RemovedShipment) {
    x.open("Документ");
    x.leaf("Ид", &r.id);
    x.leaf("Номер", &r.id);
    x.leaf("Дата", &date(r.date));
    x.leaf("ХозОперация", "Отпуск товара");
    x.leaf("Роль", "Продавец");
    x.leaf("Валюта", &currency(&o.currency));
    x.leaf("Основание", &o.id.to_string());
    x.leaf("ПометкаУдаления", "true");
    x.leaf("ЗначенияРеквизитов", "");
    x.leaf("Контрагенты", "");
    x.leaf("Товары", "");
    x.close("Документ");
}

/// Части адреса: (суффикс поля контрагента, `Тип` адресного поля) в порядке `ExportContragents`.
const ADDRESS_PARTS: &[(&str, &str)] = &[
    ("INDEX", "Почтовый индекс"),
    ("COUNTRY", "Страна"),
    ("REGION", "Регион"),
    ("STATE", "Район"),
    ("TOWN", "Населенный пункт"),
    ("CITY", "Город"),
    ("STREET", "Улица"),
    ("HOUSE", "Дом"),
    ("BUILDING", "Корпус"),
    ("FLAT", "Квартира"),
];

/// `CSaleExport::ExportContragents`.
fn contragents(x: &mut Xml, a: &Agent, v: SchemaVersion) {
    let f = |key: &str| a.fields.get(key).map(String::as_str).unwrap_or("");
    let address = |x: &mut Xml, tag: &str, prefix: &str| {
        x.open(tag);
        x.leaf("Представление", f(&format!("{prefix}ADDRESS_FULL")));
        for (key, kind) in ADDRESS_PARTS {
            let value = f(&format!("{prefix}{key}"));
            if !value.is_empty() {
                x.open("АдресноеПоле");
                x.leaf("Тип", kind);
                x.leaf("Значение", value);
                x.close("АдресноеПоле");
            }
        }
        x.close(tag);
    };
    let optional = |x: &mut Xml, tag: &str, key: &str| {
        let value = f(key);
        if !value.is_empty() {
            x.leaf(tag, value);
        }
    };

    x.open("Контрагенты");
    x.open("Контрагент");
    x.leaf("Ид", &a.id);
    x.leaf("Наименование", f("AGENT_NAME"));
    match a.domain {
        Domain::Individual => {
            x.leaf("ПолноеНаименование", f("FULL_NAME"));
            for (tag, key) in [
                ("Фамилия", "SURNAME"),
                ("Имя", "NAME"),
                ("Отчество", "SECOND_NAME"),
                ("ДатаРождения", "BIRTHDAY"),
                ("Пол", "MALE"),
                ("ИНН", "INN"),
                ("КПП", "KPP"),
            ] {
                optional(x, tag, key);
            }
            address(x, "АдресРегистрации", "");
        }
        Domain::Entity => {
            x.leaf("ОфициальноеНаименование", f("FULL_NAME"));
            address(x, "ЮридическийАдрес", "");
            for (tag, key) in [
                ("ИНН", "INN"),
                ("КПП", "KPP"),
                ("ЕГРПО", "EGRPO"),
                ("ОКВЭД", "OKVED"),
                ("ОКДП", "OKDP"),
                ("ОКОПФ", "OKOPF"),
                ("ОКФС", "OKFC"),
                ("ОКПО", "OKPO"),
            ] {
                optional(x, tag, key);
            }
            optional(x, "КодПоОКПО", "OKPO");
            if !f("ACCOUNT_NUMBER").is_empty() {
                x.open("РасчетныеСчета");
                x.open("РасчетныйСчет");
                x.leaf("НомерСчета", f("ACCOUNT_NUMBER"));
                x.open("Банк");
                x.leaf("Наименование", f("B_NAME"));
                address(x, "Адрес", "B_");
                optional(x, "БИК", "B_BIK");
                x.close("Банк");
                x.close("РасчетныйСчет");
                x.close("РасчетныеСчета");
            }
        }
    }
    if !f("F_ADDRESS_FULL").is_empty() {
        address(x, "Адрес", "F_");
    }
    let (phone, email) = (f("PHONE"), f("EMAIL"));
    if !phone.is_empty() || !email.is_empty() {
        let (phone_type, email_type) = if v.new_contacts() {
            ("Телефон рабочий", "Электронная почта")
        } else {
            ("ТелефонРабочий", "Почта")
        };
        x.open("Контакты");
        for (kind, value) in [(phone_type, phone), (email_type, email)] {
            if !value.is_empty() {
                x.open("Контакт");
                x.leaf("Тип", kind);
                x.leaf("Значение", value);
                x.close("Контакт");
            }
        }
        x.close("Контакты");
    }
    let contact = f("CONTACT_PERSON");
    if !contact.is_empty() {
        x.open("Представители");
        x.open("Представитель");
        x.open("Контрагент");
        x.leaf("Отношение", "Контактное лицо");
        x.leaf("Ид", &hex::encode(Md5::digest(contact.as_bytes())));
        x.leaf("Наименование", contact);
        x.close("Контрагент");
        x.close("Представитель");
        x.close("Представители");
    }
    x.leaf("Роль", "Покупатель");
    x.close("Контрагент");
    x.close("Контрагенты");
}

/// `Товары` (`getXmlBasketItems`) со строкой доставки (`getOrderDeliveryItem`).
fn goods(x: &mut Xml, items: &[ExportItem], delivery: Option<&ExportDelivery>, v: SchemaVersion) {
    x.open("Товары");
    for it in items {
        x.open("Товар");
        x.leaf("Ид", &it.xml_id);
        x.leaf("ИдКаталога", &it.catalog_xml_id);
        x.leaf("Наименование", &it.name);
        unit(x, &it.unit_code, &it.unit_name, v);
        x.leaf("ЦенаЗаЕдиницу", &num(it.price));
        x.leaf("Количество", &num(it.quantity));
        x.leaf("Сумма", &num(it.sum));
        nomenclature(x, "Товар");
        x.close("Товар");
    }
    if let Some(d) = delivery.filter(|d| d.price > 0.0) {
        let price = num(d.price);
        x.open("Товар");
        x.leaf("Ид", "ORDER_DELIVERY");
        x.leaf("Наименование", "Доставка заказа");
        unit(x, "796", "", v);
        x.leaf("ЦенаЗаЕдиницу", &price);
        x.leaf("Количество", "1");
        x.leaf("Сумма", &price);
        nomenclature(x, "Услуга");
        x.close("Товар");
    }
    x.close("Товары");
}

/// Единица измерения позиции (`outputXmlUnit`): после 2.05 — `Единица` с кодом (796 по
/// умолчанию) и `Коэффициент`, до — атрибуты `БазоваяЕдиница`.
fn unit(x: &mut Xml, code: &str, name: &str, v: SchemaVersion) {
    let code = code.trim();
    let is_piece = leading_number(code) == 796.0;
    if v.new_contacts() {
        let code = if leading_number(code) < 1.0 {
            "796"
        } else {
            code
        };
        let name = if name.is_empty() && leading_number(code) == 796.0 {
            "Штука"
        } else {
            name
        };
        x.open("Единица");
        x.leaf("Код", code);
        x.leaf("НаименованиеПолное", name);
        x.close("Единица");
        x.leaf("Коэффициент", "1");
    } else if is_piece {
        x.leaf_attrs(
            "БазоваяЕдиница",
            &[
                ("Код", "796"),
                ("НаименованиеПолное", "Штука"),
                ("МеждународноеСокращение", "PCE"),
            ],
            "шт",
        );
    } else {
        x.leaf_attrs(
            "БазоваяЕдиница",
            &[
                ("Код", code),
                ("НаименованиеПолное", name),
                ("МеждународноеСокращение", ""),
            ],
            name,
        );
    }
}

fn nomenclature(x: &mut Xml, kind: &str) {
    traits(
        x,
        &[
            ("ВидНоменклатуры".to_string(), kind.to_string()),
            ("ТипНоменклатуры".to_string(), kind.to_string()),
        ],
    );
}

fn traits(x: &mut Xml, list: &[(String, String)]) {
    x.open("ЗначенияРеквизитов");
    for (name, value) in list {
        x.open("ЗначениеРеквизита");
        x.leaf("Наименование", name);
        x.leaf("Значение", value);
        x.close("ЗначениеРеквизита");
    }
    x.close("ЗначенияРеквизитов");
}

/// Запись в буфер UTF-8; запись в `Vec` не падает.
struct Xml {
    w: Writer<Vec<u8>>,
}

impl Xml {
    fn new() -> Self {
        Xml {
            w: Writer::new_with_indent(Vec::new(), b'\t', 1),
        }
    }

    fn event(&mut self, e: Event<'_>) {
        self.w.write_event(e).expect("write to Vec");
    }

    fn decl(&mut self) {
        self.event(Event::Decl(BytesDecl::new(
            "1.0",
            Some("windows-1251"),
            None,
        )));
    }

    fn open(&mut self, name: &str) {
        self.event(Event::Start(BytesStart::new(name)));
    }

    fn open_attrs(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.event(Event::Start(
            BytesStart::new(name).with_attributes(attrs.iter().copied()),
        ));
    }

    fn close(&mut self, name: &str) {
        self.event(Event::End(BytesEnd::new(name)));
    }

    /// `<name>text</name>`, и пустой — парой тегов, как у Битрикса.
    fn leaf(&mut self, name: &str, text: &str) {
        self.open(name);
        self.event(Event::Text(BytesText::new(text)));
        self.close(name);
    }

    fn leaf_attrs(&mut self, name: &str, attrs: &[(&str, &str)], text: &str) {
        self.open_attrs(name, attrs);
        self.event(Event::Text(BytesText::new(text)));
        self.close(name);
    }

    fn finish(self) -> String {
        String::from_utf8(self.w.into_inner()).expect("writer gets only UTF-8")
    }
}

/// UTF-8 → windows-1251; непредставимое — `?` (как `Encoding::convertEncoding`), а не `&#…;`.
fn to_cp1251(s: &str) -> Vec<u8> {
    let mut encoder = encoding_rs::WINDOWS_1251.new_encoder();
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 8192];
    let mut src = s;
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(src, &mut buf, true);
        out.extend_from_slice(&buf[..written]);
        src = &src[read..];
        match result {
            EncoderResult::InputEmpty => break,
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(_) => out.push(b'?'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use quick_xml::Reader;
    use quick_xml::events::Event;

    use super::*;

    /// Узел разобранного XML: имя, атрибуты, текст (без пробельных промежутков), дети.
    #[derive(Debug, PartialEq)]
    struct Node {
        name: String,
        attrs: Vec<(String, String)>,
        text: String,
        children: Vec<Node>,
    }

    fn tree(xml: &str) -> Node {
        let mut reader = Reader::from_str(xml);
        let mut stack: Vec<Node> = vec![Node {
            name: String::new(),
            attrs: vec![],
            text: String::new(),
            children: vec![],
        }];
        let dec = reader.decoder();
        let start = |e: &quick_xml::events::BytesStart| Node {
            name: String::from_utf8(e.name().as_ref().to_vec()).unwrap(),
            attrs: e
                .attributes()
                .map(|a| {
                    let a = a.unwrap();
                    (
                        String::from_utf8(a.key.as_ref().to_vec()).unwrap(),
                        a.decode_and_unescape_value(dec).unwrap().into_owned(),
                    )
                })
                .collect(),
            text: String::new(),
            children: vec![],
        };
        loop {
            match reader.read_event().unwrap() {
                Event::Start(e) => stack.push(start(&e)),
                Event::Empty(e) => {
                    let n = start(&e);
                    stack.last_mut().unwrap().children.push(n);
                }
                Event::End(_) => {
                    let n = stack.pop().unwrap();
                    stack.last_mut().unwrap().children.push(n);
                }
                Event::Text(t) => {
                    let t = t.unescape().unwrap();
                    if !t.trim().is_empty() {
                        stack.last_mut().unwrap().text.push_str(&t);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        let mut root = stack.pop().unwrap();
        assert_eq!(root.children.len(), 1);
        root.children.pop().unwrap()
    }

    const DECL: &str = r#"<?xml version="1.0" encoding="windows-1251"?>"#;

    fn decode(bytes: &[u8]) -> String {
        let (text, _, bad) = encoding_rs::WINDOWS_1251.decode(bytes);
        assert!(!bad);
        text.into_owned()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn generated() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 16, 7, 5, 9).unwrap()
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| (s(k), s(v))).collect()
    }

    fn agent(id: &str, domain: Domain, fields: &[(&str, &str)]) -> Agent {
        Agent {
            id: s(id),
            domain,
            fields: fields.iter().map(|(k, v)| (s(k), s(v))).collect(),
        }
    }

    fn item(
        xml_id: &str,
        catalog: &str,
        name: &str,
        unit: (&str, &str),
        price: f64,
        qty: f64,
        sum: f64,
    ) -> ExportItem {
        ExportItem {
            xml_id: s(xml_id),
            catalog_xml_id: s(catalog),
            name: s(name),
            unit_code: s(unit.0),
            unit_name: s(unit.1),
            price,
            quantity: qty,
            sum,
        }
    }

    fn order(
        id: i64,
        created: &str,
        agent: Agent,
        items: Vec<ExportItem>,
        sum: f64,
    ) -> ExportOrder {
        ExportOrder {
            id,
            number: id.to_string(),
            created_at: at(created),
            currency: s("RUB"),
            sum,
            comment: String::new(),
            id_1c: String::new(),
            version_1c: String::new(),
            agent,
            items,
            delivery: None,
            store_xml_id: None,
            traits: vec![],
            payments: vec![],
            shipments: vec![],
            removed_shipments: vec![],
        }
    }

    fn person_205() -> ExportOrder {
        let a = agent(
            "7#ivanov#Иванов Иван Иванович",
            Domain::Individual,
            &[
                ("AGENT_NAME", "Иванов Иван Иванович"),
                ("FULL_NAME", "Иванов Иван Иванович"),
                ("SURNAME", "Иванов"),
                ("NAME", "Иван"),
                ("SECOND_NAME", "Иванович"),
                ("ADDRESS_FULL", "101000, Москва, ул. Мясницкая, д. 1, кв. 5"),
                ("INDEX", "101000"),
                ("CITY", "Москва"),
                ("STREET", "Мясницкая"),
                ("HOUSE", "1"),
                ("FLAT", "5"),
                ("PHONE", "+7 900 000-00-01"),
                ("EMAIL", "ivanov@example.com"),
                ("CONTACT_PERSON", "Иванов Иван"),
            ],
        );
        let mut o = order(
            101,
            "2026-10-15T09:30:00Z",
            a,
            vec![
                item(
                    "a1b2c3",
                    "cat-1",
                    "Перчатки нитриловые",
                    ("796", "Штука"),
                    500.0,
                    3.0,
                    1500.0,
                ),
                item(
                    "d4e5f6#7788",
                    "cat-1",
                    "Песок кварцевый",
                    ("166", "Килограмм"),
                    110.0,
                    5.0,
                    550.0,
                ),
            ],
            2350.0,
        );
        o.comment = s("Позвонить перед доставкой");
        o.version_1c = s("4");
        o.delivery = Some(ExportDelivery { price: 300.0 });
        o.traits = pairs(&[
            ("Метод оплаты ИД", "2"),
            ("Метод оплаты", "Наличные"),
            ("Заказ оплачен", "false"),
            ("Доставка разрешена", "false"),
            ("Способ доставки", "Курьер"),
            ("Сумма доставки", "300"),
            ("Отменен", "false"),
            ("Финальный статус", "false"),
            ("Статус заказа", "[N] Принят"),
            ("Статуса заказа ИД", "N"),
            ("Сайт", "[s1] Трансопт"),
            (
                "Адрес доставки",
                "101000, Москва, ул. Мясницкая, д. 1, кв. 5",
            ),
        ]);
        // Оплаты и отгрузки до 2.10 не выгружаются
        o.payments = vec![payment("1", PayOperation::Cash, &[])];
        o.removed_shipments = vec![RemovedShipment {
            id: s("9"),
            date: at("2026-10-15T09:30:00Z"),
        }];
        o
    }

    fn company_208() -> ExportOrder {
        let a = agent(
            "8#romashka#",
            Domain::Entity,
            &[
                ("AGENT_NAME", "ООО «Ромашка»"),
                (
                    "FULL_NAME",
                    "Общество с ограниченной ответственностью «Ромашка»",
                ),
                ("ADDRESS_FULL", "115114, Москва, Дербеневская наб., д. 7"),
                ("INDEX", "115114"),
                ("CITY", "Москва"),
                ("INN", "7701234567"),
                ("KPP", "770101001"),
                ("OKPO", "12345678"),
                ("ACCOUNT_NUMBER", "40702810900000000001"),
                ("B_NAME", "ПАО Сбербанк"),
                ("B_ADDRESS_FULL", "Москва, ул. Вавилова, д. 19"),
                ("B_CITY", "Москва"),
                ("B_BIK", "044525225"),
                ("F_ADDRESS_FULL", "Москва, Складской пр., д. 3"),
                ("F_CITY", "Москва"),
                ("F_STREET", "Складской пр."),
                ("PHONE", "+7 495 000-00-02"),
                ("EMAIL", "zakaz@romashka.example"),
                ("CONTACT_PERSON", "Петров Пётр"),
            ],
        );
        let mut o = order(
            102,
            "2026-10-15T14:05:30Z",
            a,
            vec![item(
                "f00d-cable",
                "cat-2",
                "Кабель ВВГ 3x2,5",
                ("006", "Метр"),
                120.0,
                100.0,
                12000.0,
            )],
            12000.0,
        );
        o.number = s("TR-102");
        o.id_1c = s("ab12-cd34");
        o.version_1c = s("3");
        o.store_xml_id = Some(s("store-main"));
        o.traits = pairs(&[
            ("Метод оплаты ИД", "3"),
            ("Метод оплаты", "Безналичный расчёт"),
            ("Заказ оплачен", "true"),
            ("Дата оплаты", "15.10.2026 18:00:00"),
            ("Сайт", "[s1] Трансопт"),
        ]);
        o
    }

    fn petrova() -> Agent {
        agent(
            "9#petrova#Петрова Мария",
            Domain::Individual,
            &[
                ("AGENT_NAME", "Петрова Мария"),
                ("FULL_NAME", "Петрова Мария"),
                ("PHONE", "+7 900 000-00-03"),
                ("EMAIL", "petrova@example.com"),
            ],
        )
    }

    fn tape() -> ExportItem {
        item(
            "beef-tape",
            "cat-1",
            "Лента сигнальная",
            ("796", "Штука"),
            1000.0,
            1.0,
            1000.0,
        )
    }

    fn payment(id: &str, op: PayOperation, traits: &[(&str, &str)]) -> ExportPayment {
        ExportPayment {
            id: s(id),
            number: s(id),
            date: at("2026-10-15T10:10:00Z"),
            operation: op,
            sum: 1250.0,
            version_1c: String::new(),
            traits: pairs(traits),
        }
    }

    fn pay_traits(ps: &str, name: &str) -> Vec<(String, String)> {
        pairs(&[
            ("Дата оплаты", ""),
            ("Отменен", "false"),
            ("Метод оплаты ИД", ps),
            ("Метод оплаты", name),
            ("Оплачено", "false"),
            ("Признак возврата", "false"),
            ("Причина к возврату", ""),
            ("Сайт", "[s1] Трансопт"),
        ])
    }

    fn ship_traits() -> Vec<(String, String)> {
        pairs(&[
            ("Сумма доставки", "250"),
            ("Дата разрешения доставки", ""),
            ("Местоположения доставки", "0000073738"),
            ("Статус отгрузки", "DN"),
            ("Отгружено", "false"),
            ("Дата отгрузки", ""),
            ("Причина отмены отгрузки", ""),
            ("Резервирование", "false"),
            ("Метод доставки ИД", "2"),
            ("Метод доставки", "Курьер"),
            ("Отменен", "false"),
            ("Дата отмены", ""),
            ("Причина отмены", ""),
            ("Ошибка отгрузки", ""),
            ("Идентификатор отправления", ""),
            ("Сайт", "[s1] Трансопт"),
        ])
    }

    fn with_documents(
        id: i64,
        version: &str,
        pay: ExportPayment,
        ship_id: &str,
        ship_number: &str,
    ) -> ExportOrder {
        let mut o = order(id, "2026-10-15T10:00:00Z", petrova(), vec![tape()], 1250.0);
        o.version_1c = s(version);
        o.delivery = Some(ExportDelivery { price: 250.0 });
        o.traits = pairs(&[
            ("Метод оплаты ИД", "3"),
            ("Метод оплаты", "Безналичный расчёт"),
            ("Заказ оплачен", "false"),
            ("Сайт", "[s1] Трансопт"),
        ]);
        o.payments = vec![ExportPayment {
            version_1c: s(version),
            ..pay
        }];
        o.shipments = vec![ExportShipment {
            id: s(ship_id),
            number: s(ship_number),
            date: at("2026-10-15T10:00:00Z"),
            sum: 1250.0,
            version_1c: s(version),
            items: vec![tape()],
            delivery: Some(ExportDelivery { price: 250.0 }),
            traits: ship_traits(),
        }];
        o
    }

    fn payment_shipment_210() -> ExportOrder {
        let mut pay = payment("55", PayOperation::Cashless, &[]);
        pay.traits = pay_traits("3", "Безналичный расчёт");
        let mut o = with_documents(103, "5", pay, "shp-1c-77", "77");
        o.removed_shipments = vec![RemovedShipment {
            id: s("shp-1c-70"),
            date: at("2026-10-14T21:30:00Z"),
        }];
        o
    }

    fn container_31() -> ExportOrder {
        let mut pay = payment("60", PayOperation::Acquiring, &[]);
        pay.date = at("2026-10-15T10:20:00Z");
        pay.traits = pay_traits("4", "Банковская карта");
        with_documents(104, "2", pay, "78", "78")
    }

    fn canceled_guest() -> ExportOrder {
        let a = agent(
            "105#guest",
            Domain::Individual,
            &[
                ("AGENT_NAME", "Сидорова Анна"),
                ("FULL_NAME", "Сидорова Анна"),
            ],
        );
        let mut o = order(
            105,
            "2026-10-14T20:59:59Z",
            a,
            vec![item(
                "c0ffee",
                "cat-1",
                "Скотч упаковочный",
                ("796", "Штука"),
                33.3,
                3.0,
                99.9,
            )],
            99.9,
        );
        o.traits = pairs(&[
            ("Заказ оплачен", "false"),
            ("Отменен", "true"),
            ("Дата отмены заказа", "15.10.2026 09:00:00"),
            ("Причина отмены", "Передумал"),
            ("Финальный статус", "false"),
        ]);
        o
    }

    #[test]
    fn version_thresholds() {
        assert_eq!(SchemaVersion::parse(None), SchemaVersion(2.05));
        assert_eq!(SchemaVersion::parse(Some("")), SchemaVersion(2.05));
        assert_eq!(SchemaVersion::parse(Some("2,08")), SchemaVersion(2.08));
        assert_eq!(SchemaVersion::parse(Some(" 2, 10 ")), SchemaVersion(2.1));
        assert!(!SchemaVersion::parse(Some("2.05")).new_contacts());
        assert!(SchemaVersion::parse(Some("2.08")).new_contacts());
        assert!(!SchemaVersion::parse(Some("2.09")).documents());
        assert!(SchemaVersion::parse(Some("2,10")).documents());
        assert!(!SchemaVersion::parse(Some("2,10")).container());
        assert!(SchemaVersion::parse(Some("3.1")).container());
        assert!(SchemaVersion::parse(Some("3")).container());
        assert_eq!(SchemaVersion::default(), SchemaVersion(2.05));
    }

    #[test]
    fn renders_reference_documents() {
        let cases: Vec<(&str, Option<&str>, ExportOrder)> = vec![
            ("export_205_person.xml", None, person_205()),
            ("export_208_company.xml", Some("2.08"), company_208()),
            (
                "export_210_payment_shipment.xml",
                Some("2.10"),
                payment_shipment_210(),
            ),
            ("export_31_container.xml", Some("3.1"), container_31()),
            ("export_canceled_guest.xml", Some("2.09"), canceled_guest()),
        ];
        for (file, version, o) in cases {
            let path = format!(
                "{}/tests/fixtures/1c/sale/{file}",
                env!("CARGO_MANIFEST_DIR")
            );
            let expected = std::fs::read_to_string(&path).unwrap();
            let out = decode(&render(&[o], SchemaVersion::parse(version), generated()));
            assert!(
                out.starts_with(DECL),
                "{file}: {}",
                out.lines().next().unwrap_or("")
            );
            assert_eq!(tree(&out), tree(&expected), "{file}");
        }
    }

    #[test]
    fn several_orders_follow_each_other() {
        let out = decode(&render(
            &[container_31(), container_31()],
            SchemaVersion(3.1),
            generated(),
        ));
        let root = tree(&out);
        assert_eq!(root.children.len(), 2);
        assert!(
            root.children
                .iter()
                .all(|c| c.name == "Контейнер" && c.children.len() == 3)
        );
    }

    #[test]
    fn cp1251_escaping_and_unmappable() {
        let mut o = canceled_guest();
        o.comment = s("№ 5 & <b> 😀");
        let out = render(&[o], SchemaVersion::default(), generated());
        let (expected, _, _) =
            encoding_rs::WINDOWS_1251.encode("<Комментарий>№ 5 &amp; &lt;b&gt; ?</Комментарий>");
        assert!(
            out.windows(expected.len()).any(|w| w == &expected[..]),
            "{}",
            decode(&out)
        );
        // Остальной текст цел, документ разбирается
        let text = decode(&out);
        assert!(text.contains("Сидорова Анна"));
        tree(&text);
    }

    fn delivery_line(version: SchemaVersion) -> Node {
        let mut o = canceled_guest();
        o.delivery = Some(ExportDelivery { price: 150.5 });
        let root = tree(&decode(&render(&[o], version, generated())));
        let doc = &root.children[0];
        let goods = doc.children.iter().find(|c| c.name == "Товары").unwrap();
        let mut lines = goods
            .children
            .iter()
            .filter(|c| c.children[0].text == "ORDER_DELIVERY");
        let line = lines.next().unwrap();
        assert!(lines.next().is_none());
        Node {
            name: line.name.clone(),
            attrs: vec![],
            text: String::new(),
            children: line
                .children
                .iter()
                .map(|c| Node {
                    name: c.name.clone(),
                    attrs: c.attrs.clone(),
                    text: c.text.clone(),
                    children: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn delivery_line_and_units_by_version() {
        let names = |n: &Node| {
            n.children
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        };
        let old = delivery_line(SchemaVersion(2.05));
        assert_eq!(
            names(&old),
            [
                "Ид",
                "Наименование",
                "БазоваяЕдиница",
                "ЦенаЗаЕдиницу",
                "Количество",
                "Сумма",
                "ЗначенияРеквизитов"
            ]
        );
        let unit = &old.children[2];
        assert_eq!(unit.text, "шт");
        assert_eq!(
            unit.attrs,
            pairs(&[
                ("Код", "796"),
                ("НаименованиеПолное", "Штука"),
                ("МеждународноеСокращение", "PCE")
            ])
        );
        assert_eq!(old.children[3].text, "150.5");

        let new = delivery_line(SchemaVersion(2.08));
        assert_eq!(
            names(&new),
            [
                "Ид",
                "Наименование",
                "Единица",
                "Коэффициент",
                "ЦенаЗаЕдиницу",
                "Количество",
                "Сумма",
                "ЗначенияРеквизитов"
            ]
        );
        assert_eq!(new.children[3].text, "1");
        let mut o = canceled_guest();
        o.delivery = Some(ExportDelivery { price: 150.5 });
        let text = decode(&render(&[o.clone()], SchemaVersion(2.08), generated()));
        assert!(text.contains("<Единица>"));
        assert!(!text.contains("БазоваяЕдиница"));

        // Нулевая доставка — без строки
        o.delivery = Some(ExportDelivery { price: 0.0 });
        let text = decode(&render(&[o], SchemaVersion(2.08), generated()));
        assert!(!text.contains("ORDER_DELIVERY"));
    }

    #[test]
    fn unit_code_defaults_to_796() {
        let mut o = canceled_guest();
        o.items[0].unit_code = String::new();
        o.items[0].unit_name = String::new();
        let text = decode(&render(&[o], SchemaVersion(2.08), generated()));
        let root = tree(&text);
        let goods = root.children[0]
            .children
            .iter()
            .find(|c| c.name == "Товары")
            .unwrap();
        let unit = goods.children[0]
            .children
            .iter()
            .find(|c| c.name == "Единица")
            .unwrap();
        assert_eq!(unit.children[0].text, "796");
        assert_eq!(unit.children[1].text, "Штука");
    }
}
