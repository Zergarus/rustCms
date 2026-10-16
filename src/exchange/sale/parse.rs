//! Разбор файла документов, присланного 1С: заказы, оплаты, отгрузки и контрагенты.
//!
//! Файл без БД превращается в типизированные структуры; ключи полей контрагента — те же, что у
//! `document::Agent.fields` (`$systemCodes` Битрикса), то есть обратное отображение `ExportContragents`.
//! Кодировка и числа — как в `exchange::cml`.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use super::document::{ADDRESS_PARTS, PayOperation};
use super::settings::Domain;
use crate::exchange::cml::{CmlError, element, flag, number};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SaleFile {
    pub agents: Vec<AgentIn>,
    /// Документ вне контейнера — группа из одного.
    pub groups: Vec<Vec<DocumentIn>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentIn {
    pub id: String,
    pub domain: Domain,
    /// Те же ключи, что у `document::Agent`.
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Order,
    Payment(PayOperation),
    Shipment,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentIn {
    pub id: String,
    pub number: String,
    /// `Номер1С`; без отдельного тега — `Ид` (`XML_1C_DOCUMENT_ID` Битрикса).
    pub id_1c: String,
    pub version: String,
    /// Дата документа (`Дата`, у Битрикса также `Дата1С`).
    pub date: String,
    pub operation: Operation,
    /// `Основание`.
    pub base: String,
    pub deleted: bool,
    pub sum: Option<f64>,
    pub agent_id: Option<String>,
    pub items: Vec<ItemIn>,
    /// `ЗначенияРеквизитов`: наименование → значение.
    pub traits: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemIn {
    pub xml_id: String,
    pub name: String,
    pub price: f64,
    pub quantity: f64,
    pub sum: f64,
    pub kind: ItemKind,
}

/// Тип строки по реквизиту «ТипНоменклатуры»; услуга — строка доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Product,
    Service,
    Unknown,
}

/// Узел XML: имя, текст и вложенные элементы.
#[derive(Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Текст прямого потомка, без пробелов по краям; нет тега — пусто.
    fn val(&self, name: &str) -> &str {
        self.child(name).map_or("", |c| c.text.trim())
    }
}

fn tree(data: &[u8]) -> Result<Node, CmlError> {
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    // Фиктивный корень держит единственный настоящий.
    let mut stack = vec![Node::default()];
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| CmlError::Xml(format!("{e} (позиция {})", reader.error_position())))?;
        let decoder = reader.decoder();
        match event {
            Event::Start(e) => {
                let (name, _) = element(&e, decoder)?;
                stack.push(Node {
                    name,
                    ..Node::default()
                });
            }
            Event::Empty(e) => {
                let (name, _) = element(&e, decoder)?;
                let node = Node {
                    name,
                    ..Node::default()
                };
                stack.last_mut().expect("корень").children.push(node);
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err(CmlError::Xml("лишний закрывающий тег".into()));
                }
                let node = stack.pop().expect("проверено");
                stack.last_mut().expect("корень").children.push(node);
            }
            Event::Text(t) => {
                let t = t.unescape().map_err(|e| CmlError::Xml(e.to_string()))?;
                stack.last_mut().expect("корень").text.push_str(&t);
            }
            Event::CData(c) => {
                let c = c.decode().map_err(|e| CmlError::Xml(e.to_string()))?;
                stack.last_mut().expect("корень").text.push_str(&c);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if stack.len() != 1 {
        return Err(CmlError::Xml("неожиданный конец файла".into()));
    }
    let mut top = stack.pop().expect("корень");
    match top.children.pop() {
        Some(root) if root.name == "КоммерческаяИнформация" => Ok(root),
        _ => Err(CmlError::NotCml),
    }
}

/// Разобрать файл документов 1С (кодировка — по объявлению XML, BOM допускается).
pub fn parse(data: &[u8]) -> Result<SaleFile, CmlError> {
    let root = tree(data)?;
    let mut out = SaleFile::default();
    for node in &root.children {
        match node.name.as_str() {
            "Контрагенты" => {
                for a in node.all("Контрагент") {
                    add_agent(&mut out.agents, agent(a));
                }
            }
            "Документ" => {
                let doc = document(node, &mut out.agents)?;
                out.groups.push(vec![doc]);
            }
            "Контейнер" => {
                let mut group = Vec::new();
                for d in node.all("Документ") {
                    group.push(document(d, &mut out.agents)?);
                }
                if !group.is_empty() {
                    out.groups.push(group);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn document(n: &Node, agents: &mut Vec<AgentIn>) -> Result<DocumentIn, CmlError> {
    let number_text = n.val("Номер");
    let id_1c = match n.val("Номер1С") {
        "" => n.val("Ид"),
        v => v,
    };
    let operation = match n.val("ХозОперация") {
        "Заказ товара" => Operation::Order,
        "Выплата безналичных денег" => {
            Operation::Payment(PayOperation::Cashless)
        }
        "Выплата наличных денег" => Operation::Payment(PayOperation::Cash),
        "Эквайринговая операция" => {
            Operation::Payment(PayOperation::Acquiring)
        }
        "Отпуск товара" => Operation::Shipment,
        other => Operation::Unknown(other.to_string()),
    };

    let mut agent_id = None;
    if let Some(list) = n.child("Контрагенты") {
        for a in list.all("Контрагент") {
            let agent = agent(a);
            if agent_id.is_none() && !agent.id.is_empty() {
                agent_id = Some(agent.id.clone());
            }
            add_agent(agents, agent);
        }
    }

    let mut items = Vec::new();
    if let Some(list) = n.child("Товары") {
        for it in list.all("Товар") {
            let xml_id = it.val("Ид").to_string();
            let kind = match trait_map(it).get("ТипНоменклатуры").map(String::as_str)
            {
                Some("Товар") => ItemKind::Product,
                Some("Услуга") => ItemKind::Service,
                _ => ItemKind::Unknown,
            };
            items.push(ItemIn {
                kind,
                xml_id,
                name: it.val("Наименование").to_string(),
                price: num(it.val("ЦенаЗаЕдиницу")),
                quantity: num(it.val("Количество")),
                sum: num(it.val("Сумма")),
            });
        }
    }

    let traits = trait_map(n);

    Ok(DocumentIn {
        id: n.val("Ид").to_string(),
        number: number_text.to_string(),
        id_1c: id_1c.to_string(),
        version: n.val("НомерВерсии").to_string(),
        date: match n.val("Дата") {
            "" => n.val("Дата1С"),
            v => v,
        }
        .to_string(),
        operation,
        base: n.val("Основание").to_string(),
        deleted: flag(n.val("ПометкаУдаления")),
        sum: number(n.val("Сумма")).unwrap_or(Some(0.0)),
        agent_id,
        items,
        traits,
    })
}

/// Как `ToFloat` Битрикса: нечисловое значение — 0, документ остаётся.
fn add_agent(agents: &mut Vec<AgentIn>, agent: AgentIn) {
    if !agent.id.is_empty() && !agents.iter().any(|x| x.id == agent.id) {
        agents.push(agent);
    }
}

fn num(s: &str) -> f64 {
    number(s).ok().flatten().unwrap_or(0.0)
}

/// `ЗначенияРеквизитов`: наименование → значение.
fn trait_map(n: &Node) -> BTreeMap<String, String> {
    let mut traits = BTreeMap::new();
    if let Some(list) = n.child("ЗначенияРеквизитов") {
        for t in list.all("ЗначениеРеквизита") {
            let name = t.val("Наименование");
            if !name.is_empty() {
                traits.insert(name.to_string(), t.val("Значение").to_string());
            }
        }
    }
    traits
}

/// Контрагент 1С в поля контрагента (обратное к `ExportContragents`); пустые значения не попадают.
fn agent(n: &Node) -> AgentIn {
    let mut fields = BTreeMap::new();

    put(&mut fields, "AGENT_NAME", n.val("Наименование"));
    let official = n.val("ОфициальноеНаименование");
    put(
        &mut fields,
        "FULL_NAME",
        if official.is_empty() {
            n.val("ПолноеНаименование")
        } else {
            official
        },
    );
    for (tag, key) in [
        ("Фамилия", "SURNAME"),
        ("Имя", "NAME"),
        ("Отчество", "SECOND_NAME"),
        ("ДатаРождения", "BIRTHDAY"),
        ("Пол", "MALE"),
        ("ИНН", "INN"),
        ("КПП", "KPP"),
        ("ЕГРПО", "EGRPO"),
        ("ОКВЭД", "OKVED"),
        ("ОКДП", "OKDP"),
        ("ОКОПФ", "OKOPF"),
        ("ОКФС", "OKFC"),
    ] {
        put(&mut fields, key, n.val(tag));
    }
    put(
        &mut fields,
        "OKPO",
        match n.val("ОКПО") {
            "" => n.val("КодПоОКПО"),
            v => v,
        },
    );

    let domain = if n.val("ИНН").is_empty() {
        Domain::Individual
    } else {
        Domain::Entity
    };
    address(&mut fields, n, "АдресРегистрации", "");
    address(&mut fields, n, "ЮридическийАдрес", "");
    address(&mut fields, n, "Адрес", "F_");

    if let Some(contacts) = n.child("Контакты") {
        for c in contacts.all("Контакт") {
            match c.val("Тип") {
                "Телефон рабочий" | "ТелефонРабочий" => {
                    put(&mut fields, "PHONE", c.val("Значение"))
                }
                "Электронная почта" | "Почта" => {
                    put(&mut fields, "EMAIL", c.val("Значение"))
                }
                _ => {}
            }
        }
    }

    if let Some(rep) = n
        .child("Представители")
        .and_then(|l| l.child("Представитель"))
        .and_then(|r| r.child("Контрагент"))
    {
        put(&mut fields, "CONTACT_PERSON", rep.val("Наименование"));
    }

    AgentIn {
        id: n.val("Ид").to_string(),
        domain,
        fields,
    }
}

fn put(fields: &mut BTreeMap<String, String>, key: &str, value: &str) {
    if !value.is_empty() {
        fields.insert(key.to_string(), value.to_string());
    }
}

/// Адрес из `Представление` и `АдресноеПоле`; `prefix` — `""`, `F_` или `B_`.
fn address(fields: &mut BTreeMap<String, String>, node: &Node, tag: &str, prefix: &str) {
    let Some(a) = node.child(tag) else { return };
    put(
        fields,
        &format!("{prefix}ADDRESS_FULL"),
        a.val("Представление"),
    );
    for f in a.all("АдресноеПоле") {
        let kind = f.val("Тип");
        if let Some((key, _)) = ADDRESS_PARTS.iter().find(|(_, k)| *k == kind) {
            put(fields, &format!("{prefix}{key}"), f.val("Значение"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(name: &str) -> SaleFile {
        let path = format!("tests/fixtures/1c/sale/{name}");
        parse(&std::fs::read(&path).unwrap()).unwrap()
    }

    fn only(file: &SaleFile) -> &DocumentIn {
        assert_eq!(file.groups.len(), 1);
        assert_eq!(file.groups[0].len(), 1);
        &file.groups[0][0]
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn order_status() {
        let file = load("import_order_status.xml");
        let d = only(&file);
        assert_eq!(d.id, "103");
        assert_eq!(d.number, "103");
        assert_eq!(d.id_1c, "ЗК-0000103");
        assert_eq!(d.version, "7");
        assert_eq!(d.operation, Operation::Order);
        assert_eq!(d.base, "");
        assert!(!d.deleted);
        assert_eq!(d.sum, Some(1250.0));
        assert_eq!(d.agent_id, None);
        assert!(d.items.is_empty());
        assert_eq!(
            d.traits,
            map(&[
                ("Статуса заказа ИД", "F"),
                ("Отменен", "false"),
                ("Проведен", "true")
            ])
        );
        assert!(file.agents.is_empty());
    }

    #[test]
    fn order_basket() {
        let file = load("import_order_basket.xml");
        let d = only(&file);
        assert_eq!(d.sum, Some(12850.5));
        assert_eq!(d.agent_id.as_deref(), Some("9#petrova#Петрова Мария"));
        assert_eq!(
            d.items,
            [
                ItemIn {
                    xml_id: "beef-tape".into(),
                    name: "Лента сигнальная".into(),
                    price: 1200.5,
                    quantity: 2.5,
                    sum: 3001.25,
                    kind: ItemKind::Product,
                },
                ItemIn {
                    xml_id: "cone-30".into(),
                    name: "Конус дорожный".into(),
                    price: 849.0,
                    quantity: 10.0,
                    sum: 8490.0,
                    kind: ItemKind::Unknown,
                },
                ItemIn {
                    xml_id: "ORDER_DELIVERY".into(),
                    name: "Доставка заказа".into(),
                    price: 250.0,
                    quantity: 1.0,
                    sum: 250.0,
                    kind: ItemKind::Service,
                },
            ]
        );
        assert_eq!(file.agents.len(), 1);
        assert_eq!(file.agents[0].domain, Domain::Individual);
        assert_eq!(
            file.agents[0].fields,
            map(&[("AGENT_NAME", "Петрова Мария")])
        );
    }

    #[test]
    fn id_1c_falls_back_to_id() {
        let file = load("import_payment.xml");
        assert_eq!(only(&file).id_1c, "ПКО-00009");
        let file = load("import_shipment.xml");
        let d = only(&file);
        assert_eq!(d.number, "77");
        assert_eq!(d.id_1c, "shp-1c-77");
    }

    #[test]
    fn domain_follows_inn_and_root_agents() {
        let file = load("import_agents.xml");
        let ids: Vec<_> = file.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["ip-5", "org-noinn", "fiz-1"]);
        let domains: Vec<_> = file.agents.iter().map(|a| a.domain).collect();
        assert_eq!(
            domains,
            [Domain::Entity, Domain::Individual, Domain::Individual]
        );
        assert_eq!(file.agents[0].fields["INN"], "500100732259");
        assert_eq!(file.agents[0].fields["PHONE"], "+7 900 555-00-05");
        assert_eq!(file.agents[2].fields["AGENT_NAME"], "Орлов Олег");
        let d = only(&file);
        assert_eq!(d.agent_id.as_deref(), Some("fiz-1"));
    }

    #[test]
    fn bad_numbers_become_zero() {
        let file = load("import_agents.xml");
        let d = only(&file);
        assert_eq!(d.sum, Some(0.0));
        assert_eq!(d.items[0].price, 0.0);
        assert_eq!(d.items[0].quantity, 2.0);
        assert_eq!(d.items[0].sum, 100.0);
    }

    #[test]
    fn bank_accounts_are_not_read() {
        let file = load("import_new_order_agent.xml");
        let f = &file.agents[0].fields;
        assert!(
            !f.keys()
                .any(|k| k.starts_with("B_") || k == "ACCOUNT_NUMBER")
        );
    }

    #[test]
    fn new_order_agents() {
        let file = load("import_new_order_agent.xml");
        let d = only(&file);
        assert_eq!(d.id, "ord-1c-501");
        assert_eq!(d.agent_id.as_deref(), Some("org-77"));
        assert_eq!(file.agents.len(), 2);

        let org = &file.agents[0];
        assert_eq!(org.id, "org-77");
        assert_eq!(org.domain, Domain::Entity);
        assert_eq!(
            org.fields,
            map(&[
                ("AGENT_NAME", "ООО Ромашка"),
                (
                    "FULL_NAME",
                    "Общество с ограниченной ответственностью «Ромашка»"
                ),
                (
                    "ADDRESS_FULL",
                    "101000, Россия, Москва, ул. Мясницкая, д. 1, кв. 5"
                ),
                ("INDEX", "101000"),
                ("COUNTRY", "Россия"),
                ("CITY", "Москва"),
                ("STREET", "ул. Мясницкая"),
                ("HOUSE", "1"),
                ("BUILDING", "2"),
                ("FLAT", "5"),
                ("INN", "7701234567"),
                ("KPP", "770101001"),
                ("OKPO", "12345678"),
                ("F_ADDRESS_FULL", "Москва, Складской пр., 3"),
                ("F_STREET", "Складской пр."),
                ("PHONE", "+7 495 000-00-01"),
                ("EMAIL", "office@romashka.example"),
                ("CONTACT_PERSON", "Иванов Пётр"),
            ])
        );

        let person = &file.agents[1];
        assert_eq!(person.id, "fiz-12");
        assert_eq!(person.domain, Domain::Individual);
        assert_eq!(
            person.fields,
            map(&[
                ("AGENT_NAME", "Сидоров Алексей Иванович"),
                ("FULL_NAME", "Сидоров Алексей Иванович"),
                ("SURNAME", "Сидоров"),
                ("NAME", "Алексей"),
                ("SECOND_NAME", "Иванович"),
                ("BIRTHDAY", "1985-03-04"),
                ("MALE", "M"),
                ("ADDRESS_FULL", "Казань, ул. Ленина, 5"),
                ("CITY", "Казань"),
                ("PHONE", "+7 900 111-22-33"),
                ("EMAIL", "sidorov@example.com"),
            ])
        );
    }

    #[test]
    fn payment() {
        let file = load("import_payment.xml");
        let d = only(&file);
        assert_eq!(d.id, "pay-1c-9");
        assert_eq!(d.number, "55");
        assert_eq!(d.version, "3");
        assert_eq!(d.operation, Operation::Payment(PayOperation::Cash));
        assert_eq!(d.base, "103");
        assert_eq!(d.sum, Some(1250.0));
        assert_eq!(
            d.traits,
            map(&[("Оплачен", "true"), ("Признак возврата", "false")])
        );
    }

    #[test]
    fn shipment() {
        let file = load("import_shipment.xml");
        let d = only(&file);
        assert_eq!(d.operation, Operation::Shipment);
        assert_eq!(d.id, "shp-1c-77");
        assert_eq!(d.base, "103");
        assert_eq!(d.date, "2026-10-16");
        assert_eq!(d.items.len(), 2);
        assert_eq!(d.items[0].kind, ItemKind::Product);
        assert_eq!(d.items[1].kind, ItemKind::Service);
        assert_eq!(d.traits["Дата отгрузки по 1С"], "2026-10-16 10:30:00");
        assert_eq!(d.traits["Номер отслеживания"], "RR123456789RU");
        assert_eq!(d.traits["Доставка разрешена"], "true");
    }

    #[test]
    fn removed_and_unknown() {
        let file = load("import_removed.xml");
        assert_eq!(file.groups.len(), 3);
        let shipment = &file.groups[0][0];
        assert_eq!(shipment.operation, Operation::Shipment);
        assert!(shipment.deleted);
        assert!(shipment.items.is_empty() && shipment.traits.is_empty());
        assert_eq!(shipment.sum, None);
        assert!(file.agents.is_empty());

        let order = &file.groups[1][0];
        assert_eq!(order.operation, Operation::Order);
        assert!(order.deleted);

        let other = &file.groups[2][0];
        assert_eq!(
            other.operation,
            Operation::Unknown("Возврат товаров".into())
        );
        assert!(!other.deleted);
    }

    #[test]
    fn container_is_one_group() {
        let file = load("import_container.xml");
        assert_eq!(file.groups.len(), 2);
        let ops: Vec<_> = file.groups[0].iter().map(|d| d.operation.clone()).collect();
        assert_eq!(
            ops,
            [
                Operation::Order,
                Operation::Payment(PayOperation::Acquiring),
                Operation::Shipment
            ]
        );
        assert!(file.groups[0].iter().skip(1).all(|d| d.base == "104"));
        assert_eq!(file.groups[1].len(), 1);
        assert_eq!(
            file.groups[1][0].operation,
            Operation::Payment(PayOperation::Cashless)
        );
        assert_eq!(file.agents.len(), 1);
    }

    #[test]
    fn windows_1251_is_decoded() {
        let bytes = std::fs::read("tests/fixtures/1c/sale/import_cp1251.xml").unwrap();
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "фикстура должна быть в cp1251"
        );
        let file = parse(&bytes).unwrap();
        let d = only(&file);
        assert_eq!(d.operation, Operation::Order);
        assert_eq!(d.items[0].name, "Лента сигнальная");
        assert_eq!(file.agents[0].id, "9#petrova#Петрова Мария");
        assert_eq!(file.agents[0].fields["AGENT_NAME"], "Петрова Мария");
    }

    #[test]
    fn utf8_bom_is_skipped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(std::fs::read("tests/fixtures/1c/sale/import_payment.xml").unwrap());
        assert_eq!(only(&parse(&bytes).unwrap()).id, "pay-1c-9");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse("<Другое/>".as_bytes()),
            Err(CmlError::NotCml)
        ));
        assert!(matches!(
            parse("<КоммерческаяИнформация><Документ>".as_bytes()),
            Err(CmlError::Xml(_))
        ));
    }
}
