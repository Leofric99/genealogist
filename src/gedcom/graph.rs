use super::parser::{Family, Gedcom, Person};
use std::collections::HashMap;

#[derive(Debug)]
pub struct FamilyGraph<'graph, 'data> {
    pub people: HashMap<&'data str, &'graph Person<'data>>,
    pub parents: HashMap<&'data str, Vec<&'data str>>,
    pub children: HashMap<&'data str, Vec<&'data str>>,
    pub spouses: HashMap<&'data str, Vec<&'data str>>,
}

impl<'graph, 'data> FamilyGraph<'graph, 'data> {
    pub fn build(gedcom: &'graph Gedcom<'data>) -> Self {
        let people: HashMap<_, _> = gedcom
            .people
            .iter()
            .map(|person| (person.xref, person))
            .collect();
        let mut graph = Self {
            people,
            parents: HashMap::new(),
            children: HashMap::new(),
            spouses: HashMap::new(),
        };
        for family in &gedcom.families {
            graph.add_family(family);
        }
        graph
    }

    fn add_family(&mut self, family: &Family<'data>) {
        let parent_ids: Vec<&'data str> = [family.husband, family.wife]
            .into_iter()
            .flatten()
            .collect();
        for parent in &parent_ids {
            for child in &family.children {
                self.parents.entry(child).or_default().push(parent);
                self.children.entry(parent).or_default().push(child);
            }
        }
        if let (Some(husband), Some(wife)) = (family.husband, family.wife) {
            self.spouses.entry(husband).or_default().push(wife);
            self.spouses.entry(wife).or_default().push(husband);
        }
    }
}
