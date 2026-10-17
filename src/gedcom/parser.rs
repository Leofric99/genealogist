#[derive(Clone, Debug, Default)]
pub struct Event<'a> {
    pub kind: &'a str,
    pub date: Option<&'a str>,
    pub place: Option<&'a str>,
}

#[derive(Clone, Debug, Default)]
pub struct Person<'a> {
    pub xref: &'a str,
    pub name: Option<&'a str>,
    pub sex: Option<&'a str>,
    pub birth: Option<Event<'a>>,
    pub death: Option<Event<'a>>,
    pub events: Vec<Event<'a>>,
}

#[derive(Clone, Debug, Default)]
pub struct Family<'a> {
    pub xref: &'a str,
    pub husband: Option<&'a str>,
    pub wife: Option<&'a str>,
    pub children: Vec<&'a str>,
}

#[derive(Clone, Debug, Default)]
pub struct Gedcom<'a> {
    pub people: Vec<Person<'a>>,
    pub families: Vec<Family<'a>>,
}

pub fn parse(input: &str) -> Gedcom<'_> {
    let mut result = Gedcom::default();
    let mut current_person: Option<usize> = None;
    let mut current_family: Option<usize> = None;
    let mut current_event: Option<usize> = None;
    let mut event_kind: Option<&str> = None;

    for line in input.lines() {
        let mut fields = line.splitn(3, ' ');
        let level = match fields.next().and_then(|value| value.parse::<u8>().ok()) {
            Some(level) => level,
            None => continue,
        };
        let second = fields.next().unwrap_or("");
        let third = fields.next().unwrap_or("").trim();

        if level == 0 {
            current_event = None;
            event_kind = None;
            if second.starts_with('@') && third == "INDI" {
                result.people.push(Person {
                    xref: second,
                    ..Person::default()
                });
                current_person = Some(result.people.len() - 1);
                current_family = None;
            } else if second.starts_with('@') && third == "FAM" {
                result.families.push(Family {
                    xref: second,
                    ..Family::default()
                });
                current_family = Some(result.families.len() - 1);
                current_person = None;
            } else {
                current_person = None;
                current_family = None;
            }
            continue;
        }

        if let Some(index) = current_person {
            if level == 1 {
                current_event = None;
                event_kind = None;
                match second {
                    "NAME" => result.people[index].name = Some(third),
                    "SEX" => result.people[index].sex = Some(third),
                    "BIRT" | "CHR" | "BAPM" | "DEAT" | "BURI" | "MARR" | "RESI" | "IMMI"
                    | "EMIG" => {
                        let event = Event {
                            kind: second,
                            ..Event::default()
                        };
                        result.people[index].events.push(event);
                        current_event = Some(result.people[index].events.len() - 1);
                        event_kind = Some(second);
                        if second == "BIRT" {
                            result.people[index].birth = Some(Event {
                                kind: second,
                                ..Event::default()
                            });
                        } else if second == "DEAT" {
                            result.people[index].death = Some(Event {
                                kind: second,
                                ..Event::default()
                            });
                        }
                    }
                    _ => {}
                }
            } else if level == 2 {
                if let Some(event_index) = current_event {
                    let event = &mut result.people[index].events[event_index];
                    match second {
                        "DATE" => event.date = Some(third),
                        "PLAC" => event.place = Some(third),
                        _ => {}
                    }
                    if Some(event.kind) == event_kind {
                        if event.kind == "BIRT" {
                            let birth = result.people[index].birth.get_or_insert_with(|| Event {
                                kind: "BIRT",
                                ..Event::default()
                            });
                            if second == "DATE" {
                                birth.date = Some(third);
                            }
                            if second == "PLAC" {
                                birth.place = Some(third);
                            }
                        } else if event.kind == "DEAT" {
                            let death = result.people[index].death.get_or_insert_with(|| Event {
                                kind: "DEAT",
                                ..Event::default()
                            });
                            if second == "DATE" {
                                death.date = Some(third);
                            }
                            if second == "PLAC" {
                                death.place = Some(third);
                            }
                        }
                    }
                }
            }
        } else if let Some(index) = current_family {
            if level == 1 {
                match second {
                    "HUSB" => result.families[index].husband = Some(third),
                    "WIFE" => result.families[index].wife = Some(third),
                    "CHIL" => result.families[index].children.push(third),
                    _ => {}
                }
            }
        }
    }
    result
}

pub fn date_year(date: &str) -> Option<i32> {
    date.split_whitespace().last()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_individual_events_and_family_links_without_copying_fields() {
        let input = "0 @I1@ INDI\n1 NAME Ada /Example/\n1 SEX F\n1 BIRT\n2 DATE 2 JAN 1900\n1 DEAT\n2 DATE 4 MAR 1980\n0 @I2@ INDI\n1 NAME Child /Example/\n1 BIRT\n2 DATE 3 FEB 1925\n0 @F1@ FAM\n1 WIFE @I1@\n1 CHIL @I2@\n";
        let parsed = parse(input);
        assert_eq!(parsed.people.len(), 2);
        assert_eq!(parsed.people[0].name, Some("Ada /Example/"));
        assert_eq!(
            parsed.people[0].birth.as_ref().and_then(|event| event.date),
            Some("2 JAN 1900")
        );
        assert_eq!(parsed.families[0].wife, Some("@I1@"));
        assert_eq!(parsed.families[0].children, vec!["@I2@"]);
    }
}
