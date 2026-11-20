//! Which part of the configuration tree a binary reads.
//!
//! A binary reads `global` and its own sections, plus a short list of named
//! settings of other sections it depends on (R11, R12). The parser does not
//! visit any other section, and resolution does not look up any other
//! setting, so an error in one service's section never stops another
//! service.

use super::settings::{Section, SettingKey};

/// A list of sections plus named extra settings of other sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    sections: &'static [Section],
    extra: &'static [SettingKey],
}

impl Scope {
    /// `narsor-server`.
    pub const SERVER: Scope = Scope::new(&[Section::Global, Section::Server], &[]);

    /// `narsor-web`: it finds the local daemon through `server.state`.
    pub const WEB: Scope = Scope::new(&[Section::Global, Section::Web], &[SettingKey::StateDir]);

    /// `narsor` (the terminal interface): it finds the local daemon through
    /// `server.state`.
    pub const TUI: Scope = Scope::new(&[Section::Global, Section::Tui], &[SettingKey::StateDir]);

    /// `narsor-cli`: it also hosts the `tui` command (R25), and finds the
    /// local daemon through `server.state`.
    pub const CLI: Scope = Scope::new(
        &[Section::Global, Section::Cli, Section::Tui],
        &[SettingKey::StateDir],
    );

    /// `narsor-runner`.
    pub const RUNNER: Scope = Scope::new(&[Section::Global, Section::Runner], &[]);

    /// `narsor-controller local`.
    pub const CONTROLLER: Scope = Scope::new(&[Section::Global, Section::Controller], &[]);

    /// Every section: for validating a whole file (`init`) and for listing
    /// every setting.
    pub const ALL: Scope = Scope::new(&Section::ALL, &[]);

    /// A scope of these sections plus these settings of other sections.
    pub const fn new(sections: &'static [Section], extra: &'static [SettingKey]) -> Scope {
        Scope { sections, extra }
    }

    /// The sections read whole.
    pub fn sections(&self) -> &'static [Section] {
        self.sections
    }

    /// The settings read from sections that are not read whole.
    pub fn extra(&self) -> &'static [SettingKey] {
        self.extra
    }

    /// Whether the whole section is read.
    pub fn includes_section(&self, section: Section) -> bool {
        self.sections.contains(&section)
    }

    /// Whether this setting is read.
    pub fn includes(&self, setting: SettingKey) -> bool {
        self.includes_section(setting.section()) || self.extra.contains(&setting)
    }

    /// The settings read, in registry order.
    pub fn settings(&self) -> impl Iterator<Item = SettingKey> + '_ {
        SettingKey::ALL
            .into_iter()
            .filter(move |setting| self.includes(*setting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_reads_its_sections_and_only_the_named_server_setting() {
        for client in [Scope::WEB, Scope::TUI, Scope::CLI] {
            assert!(client.includes(SettingKey::StateDir));
            assert!(client.includes(SettingKey::LogLevel));
            assert!(!client.includes(SettingKey::ServerMode));
            assert!(!client.includes(SettingKey::CatalogDir));
        }
        assert!(Scope::CLI.includes(SettingKey::TuiRefreshInterval));
        assert!(!Scope::SERVER.includes(SettingKey::LocalHome));
        assert_eq!(Scope::ALL.settings().count(), SettingKey::ALL.len());
    }
}
