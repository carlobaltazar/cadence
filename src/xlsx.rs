//! Minimal, dependency-free `.xlsx` writer.
//!
//! Produces a single-worksheet workbook as a *stored* (uncompressed) ZIP of a handful of XML
//! parts, using inline strings — enough for Excel / LibreOffice / Google Sheets to open a data
//! table with real typed columns. It is deliberately hand-rolled (no zip/xlsx crate) to match the
//! rest of Cadence, which avoids heavy dependencies (cf. the hand-ported LZO decompressor and the
//! dynamic wpcap binding). Numbers export as numeric cells; everything else as text.

use std::io::Write;
use std::path::Path;

/// One spreadsheet cell: a numeric value or a text value.
pub enum Cell {
    Str(String),
    Num(f64),
}

/// Write `headers` (row 1) followed by `rows` to `path` as a one-sheet .xlsx workbook.
pub fn write_sheet(path: &Path, headers: &[&str], rows: &[Vec<Cell>]) -> std::io::Result<()> {
    let sheet = build_sheet_xml(headers, rows);
    let parts: [(&str, &[u8]); 5] = [
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("xl/workbook.xml", WORKBOOK.as_bytes()),
        ("xl/_rels/workbook.xml.rels", WB_RELS.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
    ];
    let zip = build_zip(&parts);
    let mut f = std::fs::File::create(path)?;
    f.write_all(&zip)?;
    Ok(())
}

/// Excel column reference for a 0-based column and 1-based row, e.g. (0,1) -> "A1", (26,2) -> "AA2".
fn col_ref(col: usize, row: usize) -> String {
    let mut n = col + 1;
    let mut letters = String::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        letters.insert(0, (b'A' + rem as u8) as char);
        n = (n - 1) / 26;
    }
    format!("{}{}", letters, row)
}

/// Escape the five XML metacharacters so names/guilds with `&`, `<`, quotes, etc. stay well-formed.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn build_sheet_xml(headers: &[&str], rows: &[Vec<Cell>]) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    s.push_str("<row r=\"1\">");
    for (c, h) in headers.iter().enumerate() {
        s.push_str(&format!(
            r#"<c r="{}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
            col_ref(c, 1),
            esc(h)
        ));
    }
    s.push_str("</row>");
    for (i, row) in rows.iter().enumerate() {
        let r = i + 2; // row 1 is the header
        s.push_str(&format!("<row r=\"{}\">", r));
        for (c, cell) in row.iter().enumerate() {
            match cell {
                Cell::Str(v) => s.push_str(&format!(
                    r#"<c r="{}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                    col_ref(c, r),
                    esc(v)
                )),
                Cell::Num(v) => {
                    s.push_str(&format!(r#"<c r="{}"><v>{}</v></c>"#, col_ref(c, r), v))
                }
            }
        }
        s.push_str("</row>");
    }
    s.push_str("</sheetData></worksheet>");
    s
}

// ---- Fixed OPC parts (workbook skeleton) --------------------------------------------------------

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Players" sheetId="1" r:id="rId1"/></sheets></workbook>"#;

const WB_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;

// ---- Stored-ZIP container -----------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Build a ZIP archive with every entry stored (compression method 0). Sufficient for .xlsx —
/// Excel reads uncompressed packages fine, and the payload here is small.
fn build_zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(parts.len());
    let mut crcs: Vec<u32> = Vec::with_capacity(parts.len());

    for (name, data) in parts {
        offsets.push(out.len() as u32);
        let crc = crc32(data);
        crcs.push(crc);
        let nb = name.as_bytes();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header sig
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: store
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed size
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(nb);
        out.extend_from_slice(data);
    }

    let cd_start = out.len() as u32;
    for (i, (name, data)) in parts.iter().enumerate() {
        let nb = name.as_bytes();
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central dir header sig
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method: store
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crcs[i].to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offsets[i].to_le_bytes());
        central.extend_from_slice(nb);
    }

    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of central dir sig
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with cd
    out.extend_from_slice(&(parts.len() as u16).to_le_bytes()); // entries this disk
    out.extend_from_slice(&(parts.len() as u16).to_le_bytes()); // total entries
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_refs() {
        assert_eq!(col_ref(0, 1), "A1");
        assert_eq!(col_ref(25, 3), "Z3");
        assert_eq!(col_ref(26, 2), "AA2");
    }

    #[test]
    fn writes_openable_package() {
        let path = std::env::temp_dir().join("cadence_xlsx_test.xlsx");
        let rows = vec![
            vec![Cell::Str("Alice & Co <1>".into()), Cell::Num(42.0)],
            vec![Cell::Str("Bob".into()), Cell::Num(7.5)],
        ];
        write_sheet(&path, &["Name", "Level"], &rows).expect("write");
        let bytes = std::fs::read(&path).expect("read back");
        // Local file header signature ("PK\x03\x04") and a trailing end-of-central-directory record.
        assert_eq!(&bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        assert!(bytes.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]));
        let _ = std::fs::remove_file(&path);
    }
}
