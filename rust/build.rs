//! Windows only: give astralguard.exe its icon and version information.
//!
//! Writes a compiled resource file (.res) directly, so no resource compiler (rc.exe,
//! windres, llvm-rc) is needed on any toolchain, and hands it to the linker. Both MSVC
//! link.exe and rust-lld (which the GNU build uses) take .res files as linker input.
//! The icon is assets/icon/astralguard.ico; regenerate it with assets/icon/render.py.

use std::path::PathBuf;

const ICON: &str = "assets/icon/astralguard.ico";

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::fs::read(ICON).expect("read the icon");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut res = Res::default();
    res.add_icon_group(&ico);
    res.add_version(&version);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("astralguard.res");
    std::fs::write(&out, res.bytes).expect("write the .res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const LANG_EN_US: u16 = 0x0409;

/// A .res file: a list of (type, id, data) records, each with a fixed header.
struct Res {
    bytes: Vec<u8>,
}

impl Default for Res {
    fn default() -> Self {
        // Every .res starts with an empty record (type 0, id 0) that marks the format.
        let mut r = Res { bytes: Vec::new() };
        r.add(0, 0, 0, 0, &[]);
        r
    }
}

impl Res {
    fn add(&mut self, kind: u16, id: u16, flags: u16, lang: u16, data: &[u8]) {
        let b = &mut self.bytes;
        b.extend((data.len() as u32).to_le_bytes()); // DataSize
        b.extend(32u32.to_le_bytes()); // HeaderSize
        b.extend([0xFF, 0xFF]);
        b.extend(kind.to_le_bytes()); // TYPE as an ordinal
        b.extend([0xFF, 0xFF]);
        b.extend(id.to_le_bytes()); // NAME as an ordinal
        b.extend(0u32.to_le_bytes()); // DataVersion
        b.extend(flags.to_le_bytes()); // MemoryFlags
        b.extend(lang.to_le_bytes()); // LanguageId
        b.extend(0u32.to_le_bytes()); // Version
        b.extend(0u32.to_le_bytes()); // Characteristics
        b.extend(data);
        pad4(b);
    }

    /// An .ico file becomes one RT_ICON per image plus the RT_GROUP_ICON (id 1) that
    /// Explorer shows: the group is the .ico directory with resource ids in place of
    /// file offsets.
    fn add_icon_group(&mut self, ico: &[u8]) {
        let u16_at = |o: usize| u16::from_le_bytes([ico[o], ico[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([ico[o], ico[o + 1], ico[o + 2], ico[o + 3]]);
        assert!(u16_at(0) == 0 && u16_at(2) == 1, "not an .ico file");
        let count = u16_at(4) as usize;
        let mut group = Vec::new();
        group.extend(0u16.to_le_bytes());
        group.extend(1u16.to_le_bytes());
        group.extend((count as u16).to_le_bytes());
        for i in 0..count {
            let e = 6 + 16 * i;
            let size = u32_at(e + 8) as usize;
            let offset = u32_at(e + 12) as usize;
            let id = (i + 1) as u16;
            self.add(RT_ICON, id, 0x1010, LANG_EN_US, &ico[offset..offset + size]);
            group.extend(&ico[e..e + 12]); // width, height, colours, reserved, planes, bpp, size
            group.extend(id.to_le_bytes());
        }
        self.add(RT_GROUP_ICON, 1, 0x1030, LANG_EN_US, &group);
    }

    /// VS_VERSIONINFO: what Explorer's Details tab and Task Manager show.
    fn add_version(&mut self, version: &str) {
        let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
        let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        let ms = ((major as u32) << 16) | minor as u32;
        let ls = (patch as u32) << 16;
        let mut fixed = Vec::new();
        for v in [
            0xFEEF_04BDu32, // signature
            0x0001_0000,    // structure version
            ms,
            ls, // file version
            ms,
            ls,          // product version
            0x3F,        // flags mask
            0,           // flags
            0x0004_0004, // VOS_NT_WINDOWS32
            1,           // VFT_APP
            0,
            0,
            0,
        ] {
            fixed.extend(v.to_le_bytes());
        }
        let strings: Vec<Vec<u8>> = [
            ("CompanyName", "Szabolcs Gergely"),
            ("FileDescription", "AstralGuard"),
            ("FileVersion", version),
            ("InternalName", "astralguard"),
            ("LegalCopyright", "Copyright (c) 2026 Szabolcs Gergely. MIT License."),
            ("OriginalFilename", "astralguard.exe"),
            ("ProductName", "AstralGuard"),
            ("ProductVersion", version),
        ]
        .iter()
        .map(|(k, v)| {
            let value = wide(v);
            node(k, &value, (value.len() / 2) as u16, 1, &[])
        })
        .collect();
        let table = node("040904B0", &[], 0, 1, &strings);
        let string_info = node("StringFileInfo", &[], 0, 1, &[table]);
        let translation = node("Translation", &[0x09, 0x04, 0xB0, 0x04], 4, 0, &[]);
        let var_info = node("VarFileInfo", &[], 0, 1, &[translation]);
        let root = node("VS_VERSION_INFO", &fixed, fixed.len() as u16, 0, &[string_info, var_info]);
        self.add(RT_VERSION, 1, 0x0030, LANG_EN_US, &root);
    }
}

/// UTF-16LE with the terminating NUL.
fn wide(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect()
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}

/// One block of the version resource: length, value length, type, key, value, children,
/// each part aligned to 4 bytes.
fn node(key: &str, value: &[u8], value_len: u16, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend(value_len.to_le_bytes());
    b.extend(kind.to_le_bytes());
    b.extend(wide(key));
    pad4(&mut b);
    b.extend(value);
    for child in children {
        pad4(&mut b);
        b.extend(child);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}
