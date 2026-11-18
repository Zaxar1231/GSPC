//! Кнопки геймпада XInput: битовая маска состояния → изменения кнопок.
//!
//! Опрос устройства (`XInputGetState`) — в хостовом слое, на Windows; здесь только разбор
//! маски, который можно проверить без геймпада. Стики в маске не учитываются.

use crate::profile::PAD_BUTTONS;

/// Биты `wButtons` из XInput, в порядке [`PAD_BUTTONS`] (кроме направлений D-pad, которые
/// идут в конце списка).
const BITS: [(&str, u16); 14] = [
    ("A", 0x1000),
    ("B", 0x2000),
    ("X", 0x4000),
    ("Y", 0x8000),
    ("LB", 0x0100),
    ("RB", 0x0200),
    ("Back", 0x0020),
    ("Start", 0x0010),
    ("LS", 0x0040),
    ("RS", 0x0080),
    ("DPadUp", 0x0001),
    ("DPadDown", 0x0002),
    ("DPadLeft", 0x0004),
    ("DPadRight", 0x0008),
];

/// Изменения кнопок между прошлым и новым состоянием: `(имя кнопки, нажата)`.
#[must_use]
pub fn changes(prev: u16, now: u16) -> Vec<(&'static str, bool)> {
    BITS.iter()
        .filter_map(|(name, bit)| {
            let (was, is) = (prev & bit != 0, now & bit != 0);
            (was != is).then_some((*name, is))
        })
        .collect()
}

/// Все имена из таблицы совпадают с [`PAD_BUTTONS`].
#[must_use]
pub fn names_match_profile() -> bool {
    BITS.iter().all(|(n, _)| PAD_BUTTONS.contains(n)) && BITS.len() == PAD_BUTTONS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_changes() {
        assert_eq!(changes(0, 0x1000), vec![("A", true)]);
        assert_eq!(changes(0x1000, 0), vec![("A", false)]);
        assert!(changes(0x1000, 0x1000).is_empty());
        let c = changes(0, 0x1000 | 0x0001);
        assert!(c.contains(&("A", true)) && c.contains(&("DPadUp", true)));
    }

    #[test]
    fn table_matches_profile_names() {
        assert!(names_match_profile());
    }
}
