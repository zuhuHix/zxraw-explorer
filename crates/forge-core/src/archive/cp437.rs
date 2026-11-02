//! IBM code page 437, the ZIP default name encoding (entries without the UTF-8 flag).

/// Upper half (0x80..=0xFF) of the standard CP437 table, indexed by `byte - 0x80`.
#[rustfmt::skip]
const HIGH: [char; 128] = [
    // 0x80
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}', // Ç ü é â ä à å ç
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}', // ê ë è ï î ì Ä Å
    // 0x90
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}', // É æ Æ ô ö ò û ù
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}', // ÿ Ö Ü ¢ £ ¥ ₧ ƒ
    // 0xA0
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}', // á í ó ú ñ Ñ ª º
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}', // ¿ ⌐ ¬ ½ ¼ ¡ « »
    // 0xB0
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}', // ░ ▒ ▓ │ ┤ ╡ ╢ ╖
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}', // ╕ ╣ ║ ╗ ╝ ╜ ╛ ┐
    // 0xC0
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}', // └ ┴ ┬ ├ ─ ┼ ╞ ╟
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}', // ╚ ╔ ╩ ╦ ╠ ═ ╬ ╧
    // 0xD0
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}', // ╨ ╤ ╥ ╙ ╘ ╒ ╓ ╫
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}', // ╪ ┘ ┌ █ ▄ ▌ ▐ ▀
    // 0xE0
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}', // α ß Γ π Σ σ µ τ
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}', // Φ Θ Ω δ ∞ φ ε ∩
    // 0xF0
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}', // ≡ ± ≥ ≤ ⌠ ⌡ ÷ ≈
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}', // ° ∙ · √ ⁿ ² ■ NBSP
];

/// Decode CP437 bytes. Bytes 0x00..=0x7F map to ASCII unchanged (we do NOT use the graphic
/// glyphs for control codes; control bytes stay as their ASCII value so path validation can reject them).
/// Bytes 0x80..=0xFF map through the standard CP437 table.
pub(crate) fn decode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                char::from(b)
            } else {
                HIGH[usize::from(b - 0x80)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ascii_passes_through_unchanged() {
        let all_ascii: Vec<u8> = (0x00..=0x7F).collect();
        let decoded = decode(&all_ascii);
        assert_eq!(decoded.chars().count(), 128);
        for (i, c) in decoded.chars().enumerate() {
            assert_eq!(c as u32, i as u32);
        }
        assert_eq!(decode(b"folder/file name.txt"), "folder/file name.txt");
    }

    #[test]
    fn high_bytes_decode_through_table() {
        assert_eq!(decode(b"Gr\x81\xE1e"), "Grüße");
    }

    #[test]
    fn all_256_bytes_decode_to_256_chars() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&all).chars().count(), 256);
    }

    #[test]
    fn upper_half_has_no_duplicate_chars() {
        let set: HashSet<char> = HIGH.iter().copied().collect();
        assert_eq!(set.len(), 128);
    }

    #[test]
    fn spot_checks() {
        assert_eq!(decode(&[0x80]), "Ç");
        assert_eq!(decode(&[0x81]), "ü");
        assert_eq!(decode(&[0x82]), "é");
        assert_eq!(decode(&[0x84]), "ä");
        assert_eq!(decode(&[0x8E]), "Ä");
        assert_eq!(decode(&[0x94]), "ö");
        assert_eq!(decode(&[0x99]), "Ö");
        assert_eq!(decode(&[0x9A]), "Ü");
        assert_eq!(decode(&[0xA4]), "ñ");
        assert_eq!(decode(&[0xB0]), "░");
        assert_eq!(decode(&[0xC4]), "─");
        assert_eq!(decode(&[0xDB]), "█");
        assert_eq!(decode(&[0xE0]), "α");
        assert_eq!(decode(&[0xE1]), "ß");
        assert_eq!(decode(&[0xE3]), "π");
        assert_eq!(decode(&[0xF8]), "°");
        assert_eq!(decode(&[0xFD]), "²");
        assert_eq!(decode(&[0xFE]), "■");
        assert_eq!(decode(&[0xFF]), "\u{00A0}");
    }
}
