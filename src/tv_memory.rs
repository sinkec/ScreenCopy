//! What this computer remembers about TVs it has shared with: the last one used,
//! and the formats each one reported, which a TV tells only during a connection.
//! The file is private and stays on this computer.

use std::fs;

use tracing::warn;

use crate::settings::{config_file, write_private_file};

const FILE_NAME: &str = "tvs";
const MAX_FILE_LENGTH: usize = 16 * 1024;
const MAX_TVS: usize = 16;
const MAX_FORMATS: usize = 16;
const FIELD_SEPARATOR: char = '\t';
const FORMAT_SEPARATOR: char = ',';

/// A picture format, as the TV reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TvFormat {
    pub height: u32,
    pub frame_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownTv {
    pub hw_address: String,
    pub name: String,
    /// In order of preference.
    pub formats: Vec<TvFormat>,
}

impl KnownTv {
    /// A TV can change its hardware address between sessions, so the name also
    /// counts. The address comes first where both could match, in `TvMemory::find`.
    fn is(&self, hw_address: &str, name: &str) -> bool {
        self.hw_address == hw_address || self.name == name
    }
}

/// The most recently used TV comes first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvMemory {
    tvs: Vec<KnownTv>,
}

impl TvMemory {
    /// An unreadable or damaged file simply means that no TV is known.
    pub fn load() -> Self {
        config_file(FILE_NAME)
            .and_then(|path| fs::read_to_string(path).ok())
            .filter(|text| text.len() <= MAX_FILE_LENGTH)
            .map_or_else(Self::default, |text| Self::parse(&text))
    }

    fn save(&self) {
        let Some(path) = config_file(FILE_NAME) else {
            warn!("no directory for the list of TVs");
            return;
        };
        if let Err(error) = write_private_file(&path, self.format().as_bytes()) {
            warn!(%error, "cannot save the list of TVs");
        }
    }

    pub fn last(&self) -> Option<&KnownTv> {
        self.tvs.first()
    }

    pub fn find(&self, hw_address: &str, name: &str) -> Option<&KnownTv> {
        self.tvs
            .iter()
            .find(|tv| tv.hw_address == hw_address)
            .or_else(|| self.tvs.iter().find(|tv| tv.name == name))
    }

    /// Makes this TV the most recently used one, and saves.
    pub fn remember(&mut self, hw_address: &str, name: &str, formats: Vec<TvFormat>) {
        self.add(hw_address, name, formats);
        self.save();
    }

    fn add(&mut self, hw_address: &str, name: &str, formats: Vec<TvFormat>) {
        self.tvs.retain(|tv| !tv.is(hw_address, name));
        self.tvs.insert(
            0,
            KnownTv {
                hw_address: hw_address.to_owned(),
                name: clean_name(name),
                formats,
            },
        );
        self.tvs.truncate(MAX_TVS);
    }

    /// Skips whatever it cannot read, line by line.
    fn parse(text: &str) -> Self {
        let tvs = text.lines().filter_map(parse_tv).take(MAX_TVS).collect();
        Self { tvs }
    }

    fn format(&self) -> String {
        self.tvs
            .iter()
            .map(|tv| {
                let formats: Vec<String> = tv
                    .formats
                    .iter()
                    .map(|format| format!("{}@{}", format.height, format.frame_rate))
                    .collect();
                format!(
                    "{}{FIELD_SEPARATOR}{}{FIELD_SEPARATOR}{}\n",
                    tv.hw_address,
                    formats.join(","),
                    tv.name,
                )
            })
            .collect()
    }
}

/// Names come over the air: they must not be able to break the file's lines.
fn clean_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn parse_tv(line: &str) -> Option<KnownTv> {
    let mut fields = line.splitn(3, FIELD_SEPARATOR);
    let hw_address = fields.next()?;
    let formats = fields.next()?;
    let name = fields.next()?;
    if !crate::p2p::is_valid_hw_address(hw_address) || name.is_empty() {
        return None;
    }
    Some(KnownTv {
        hw_address: hw_address.to_owned(),
        name: name.to_owned(),
        formats: formats
            .split(FORMAT_SEPARATOR)
            .filter_map(parse_format)
            .take(MAX_FORMATS)
            .collect(),
    })
}

fn parse_format(text: &str) -> Option<TvFormat> {
    let (height, frame_rate) = text.split_once('@')?;
    Some(TvFormat {
        height: height.parse().ok()?,
        frame_rate: frame_rate.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: TvFormat = TvFormat {
        height: 720,
        frame_rate: 30,
    };
    const FULL_HD: TvFormat = TvFormat {
        height: 1080,
        frame_rate: 30,
    };
    const ADDRESS_A: &str = "02:00:00:00:00:0a";
    const ADDRESS_B: &str = "02:00:00:00:00:0b";

    #[test]
    fn the_list_survives_saving_and_loading() {
        let mut memory = TvMemory::default();
        memory.add(ADDRESS_A, "Living Room", vec![FULL_HD, HD]);
        memory.add(ADDRESS_B, "Bedroom\tTV", vec![]);
        let loaded = TvMemory::parse(&memory.format());
        assert_eq!(loaded.last().unwrap().name, "Bedroom TV");
        assert_eq!(
            loaded.find(ADDRESS_A, "").unwrap().formats,
            vec![FULL_HD, HD]
        );
    }

    #[test]
    fn the_last_used_tv_comes_first_and_is_not_listed_twice() {
        let mut memory = TvMemory::default();
        memory.add(ADDRESS_A, "A", vec![]);
        memory.add(ADDRESS_B, "B", vec![]);
        memory.add(ADDRESS_A, "A", vec![HD]);
        assert_eq!(memory.tvs.len(), 2);
        assert_eq!(memory.last().unwrap().formats, vec![HD]);
    }

    #[test]
    fn a_tv_is_found_by_name_when_its_address_changed() {
        let mut memory = TvMemory::default();
        memory.add(ADDRESS_A, "Living Room", vec![HD]);
        let found = memory.find(ADDRESS_B, "Living Room").unwrap();
        assert_eq!(found.hw_address, ADDRESS_A);
    }

    #[test]
    fn the_address_comes_before_the_name() {
        let mut memory = TvMemory::default();
        memory.add(ADDRESS_A, "Same", vec![HD]);
        memory.tvs.push(KnownTv {
            hw_address: ADDRESS_B.to_owned(),
            name: "Same".to_owned(),
            formats: vec![FULL_HD],
        });
        assert_eq!(
            memory.find(ADDRESS_B, "Same").unwrap().formats,
            vec![FULL_HD]
        );
    }

    #[test]
    fn damaged_lines_are_skipped() {
        let text = "nonsense\n\
                    not-an-address\t1080@30\tName\n\
                    02:00:00:00:00:0a\t\t\n\
                    02:00:00:00:00:0b\tabc,1080@x,720@30\tGood\n";
        let memory = TvMemory::parse(text);
        assert_eq!(memory.tvs.len(), 1);
        assert_eq!(memory.last().unwrap().name, "Good");
        assert_eq!(memory.last().unwrap().formats, vec![HD]);
    }

    #[test]
    fn the_list_has_a_limit() {
        let line = "02:00:00:00:00:0a\t\tName\n";
        assert_eq!(TvMemory::parse(&line.repeat(100)).tvs.len(), MAX_TVS);
    }
}
